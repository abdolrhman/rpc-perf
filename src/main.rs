// Copyright 2019-2020 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

mod admin;
mod client;
mod codec;
mod common;
mod config;
mod session;
mod stats;

#[macro_use]
extern crate rustcommon_logger;

use crate::common::*;

use crate::client::*;
use crate::codec::Codec;
use crate::config::Config;
use crate::config::Protocol;
use crate::stats::{Metrics, Stat};

use rand::thread_rng;
use rustcommon_atomics::{Atomic, AtomicBool, Ordering};
use rustcommon_logger::Logger;
use rustcommon_ratelimiter::Ratelimiter;

use std::convert::TryInto;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn main() {
    let config = config::Config::new();

    Logger::new()
        .label("rpc_perf")
        .level(config.logging())
        .init()
        .expect("Failed to initialize logger");

    let config = Arc::new(config);
    let metrics = Arc::new(Metrics::new(config.clone()));

    trace!("launching stdout stats");
    let mut stats_stdout = stats::StandardOut::new(
        metrics.clone(),
        Duration::new(config.interval().try_into().unwrap(), 0),
    );

    if let Some(stats_listen) = config.listen() {
        trace!("launching http stats");
        let mut stats_http = stats::Http::new(stats_listen, metrics.inner(), None);
        let _ = thread::Builder::new()
            .name("http".to_string())
            .spawn(move || loop {
                stats_http.run();
            });
    }

    info!("rpc-perf {} initializing...", VERSION);

    config.print();

    do_warmup(config.clone(), &metrics);

    let control = Arc::new(AtomicBool::new(true));

    let request_ratelimiter = if let Some(limit) = config.request_ratelimit() {
        let ratelimiter = Ratelimiter::new(config.clients() as u64, 1, limit as u64);
        ratelimiter.set_strategy(config.request_distribution());
        Some(Arc::new(ratelimiter))
    } else {
        None
    };

    let connect_ratelimiter = if let Some(limit) = config.connect_ratelimit() {
        Some(Arc::new(Ratelimiter::new(
            config.clients() as u64,
            1,
            limit as u64,
        )))
    } else {
        None
    };

    let close_rate = if let Some(rate) = config.close_rate() {
        Some(Arc::new(Ratelimiter::new(
            config.clients() as u64,
            1,
            rate as u64,
        )))
    } else {
        None
    };

    let client_config = ClientConfig {
        config: config.clone(),
        metrics: metrics.clone(),
        control: control.clone(),
        request_ratelimiter,
        connect_ratelimiter,
        close_rate,
    };

    let mut next = Instant::now() + Duration::new(config.interval() as u64, 0);

    launch_clients(client_config.clone());

    if let Some(listen) = config.admin() {
        let mut admin_http = admin::Http::new(listen, client_config);
        let _ = thread::Builder::new()
            .name("admin".to_string())
            .spawn(move || loop {
                admin_http.run();
            });
    }

    loop {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(std::time::Duration::from_millis(1));
        } else {
            metrics.increment(&Stat::Window);
            stats_stdout.print();

            if let Some(max_window) = config.windows() {
                if metrics.reading(&Stat::Window).unwrap() >= max_window as u64 {
                    control.store(false, Ordering::SeqCst);
                    break;
                }
            }

            next += Duration::new(config.interval() as u64, 0);
        }
    }
    if let Some(waterfall) = config.waterfall() {
        metrics.save_waterfall(waterfall);
    }
}

fn do_warmup(config: Arc<Config>, metrics: &Arc<Metrics>) {
    if let Some(target) = config.warmup_hitrate() {
        info!("-----");
        info!("Warming the cache...");
        let control = Arc::new(AtomicBool::new(true));

        let client_config = ClientConfig {
            config: config.clone(),
            metrics: metrics.clone(),
            control: control.clone(),
            request_ratelimiter: None,
            connect_ratelimiter: None,
            close_rate: None,
        };

        launch_clients(client_config);

        let mut warm = 0;
        loop {
            std::thread::sleep(std::time::Duration::new(config.interval() as u64, 0));
            metrics.increment(&Stat::Window);

            let hit = metrics.reading(&Stat::ResponsesHit).unwrap_or(0) as f64;
            let miss = metrics.reading(&Stat::ResponsesMiss).unwrap_or(0) as f64;
            let hitrate = hit / (hit + miss);

            debug!("Hit-rate: {:.2}%", hitrate * 100.0);
            if hitrate >= target {
                warm += 1;
            } else {
                warm = 0;
            }

            if warm >= 3 {
                metrics.zero();
                control.store(false, Ordering::SeqCst);
                break;
            }

            metrics.zero();
        }

        info!("Warmup complete.");
    }
}

#[derive(Clone)]
pub(crate) struct ClientConfig {
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    control: Arc<AtomicBool>,
    request_ratelimiter: Option<Arc<Ratelimiter>>,
    connect_ratelimiter: Option<Arc<Ratelimiter>>,
    close_rate: Option<Arc<Ratelimiter>>,
}

fn launch_clients(config: ClientConfig) {
    let control = config.control.clone();
    let metrics = config.metrics.clone();

    for i in 0..config.config.clients() {
        let request_ratelimiter = config.request_ratelimiter.clone();
        let connect_ratelimiter = config.connect_ratelimiter.clone();
        let close_rate = config.close_rate.clone();
        let config = config.config.clone();
        let mut codec: Box<dyn Codec> = match config.protocol() {
            Protocol::Echo => Box::new(crate::codec::Echo::new()),
            Protocol::Memcache => Box::new(crate::codec::Memcache::new()),
            Protocol::ThriftCache => Box::new(crate::codec::ThriftCache::new()),
            Protocol::PelikanRds => Box::new(crate::codec::PelikanRds::new()),
            Protocol::Ping => Box::new(crate::codec::Ping::new()),
            Protocol::RedisResp => {
                Box::new(crate::codec::Redis::new(crate::codec::RedisMode::Resp))
            }
            Protocol::RedisInline => {
                Box::new(crate::codec::Redis::new(crate::codec::RedisMode::Inline))
            }
        };

        // TODO: use a different generator for warmup
        codec.set_generator(config.generator());
        codec.set_metrics(metrics.clone());

        let mut client = Client::new(
            i,
            config.clone(),
            connect_ratelimiter,
            request_ratelimiter,
            close_rate,
            metrics.clone(),
        );

        let endpoints = config.endpoints();

        for endpoint in endpoints {
            client.add_endpoint(&endpoint);
        }

        let control = control.clone();
        let _ = thread::Builder::new()
            .name(format!("client{}", i).to_string())
            .spawn(move || {
                let mut rng = thread_rng();
                while control.load(Ordering::SeqCst) {
                    client.run(&mut rng);
                }
            });
    }
}
