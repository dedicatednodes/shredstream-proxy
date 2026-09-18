use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{sleep, Builder, JoinHandle},
    time::Duration,
};

use crossbeam_channel::Receiver;
use jito_protos::shredstream::{shredstream_client::ShredstreamClient, Heartbeat};
use log::{error, info, warn};
use solana_metrics::{datapoint_info, datapoint_warn};
use tokio::runtime::Runtime;
use tonic::{codegen::InterceptedService, transport::Channel, Code};

use crate::{
    api_key_interceptor::{create_grpc_channel, ApiKeyInterceptor},
    forwarder::ShredMetrics,
    ShredstreamProxyError,
};

const REFUSED_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const UNAUTH_RETRY_INTERVAL: Duration = Duration::from_secs(15);

fn refusal_retry(code: Code) -> Option<Duration> {
    match code {
        Code::ResourceExhausted
        | Code::PermissionDenied
        | Code::FailedPrecondition
        | Code::InvalidArgument => Some(REFUSED_RETRY_INTERVAL),
        Code::Unauthenticated => Some(UNAUTH_RETRY_INTERVAL),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn heartbeat_loop_thread(
    localshred_url: String,
    api_key_header: String,
    api_key: String,
    recv_socket: SocketAddr,
    runtime: Runtime,
    metrics: Arc<ShredMetrics>,
    shutdown_receiver: Receiver<()>,
    exit: Arc<AtomicBool>,
) -> JoinHandle<()> {
    Builder::new().name("ssPxyHbeatLoop".to_string()).spawn(move || {
        let heartbeat_socket = jito_protos::shared::Socket {
            ip: recv_socket.ip().to_string(),
            port: recv_socket.port() as i64,
        };
        let mut heartbeat_interval = Duration::from_secs(1); //start with 1s, change based on server suggestion
        // use tick() since we want to avoid thread::sleep(), as it's not interruptible. want to be interruptible for exiting quickly
        let mut heartbeat_tick = crossbeam_channel::tick(heartbeat_interval);
        let metrics_tick = crossbeam_channel::tick(Duration::from_secs(30));
        let mut last_cumulative_received_shred_count = 0;
        let mut client_restart_count = 0u64;
        let mut successful_heartbeat_count = 0u64;
        let mut failed_heartbeat_count = 0u64;
        let mut client_restart_count_cumulative = 0u64;
        let mut successful_heartbeat_count_cumulative = 0u64;
        let mut failed_heartbeat_count_cumulative = 0u64;
        let mut terminal_reported = false;

        while !exit.load(Ordering::Relaxed) {
            info!("Starting heartbeat client");
            let shredstream_client_res = runtime.block_on(
                get_grpc_client(
                    localshred_url.clone(),
                    api_key_header.clone(),
                    api_key.clone(),
                )
            );
            let mut shredstream_client = match shredstream_client_res {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to connect to block engine, retrying. Error: {e}");
                    client_restart_count += 1;
                    datapoint_warn!(
                        "localshred_lite_proxy-heartbeat_client_error",
                        "localshred_url" => localshred_url,
                        ("errors", 1, i64),
                        ("error_str", e.to_string(), String),
                    );
                    sleep(Duration::from_secs(5));
                    continue; // avoid sending heartbeat, try acquiring grpc client again
                }
            };
            while !exit.load(Ordering::Relaxed) {
                crossbeam_channel::select! {
                    // send heartbeat
                    recv(heartbeat_tick) -> _ => {
                        let heartbeat_result = runtime.block_on(shredstream_client
                            .send_heartbeat(Heartbeat {
                                socket: Some(heartbeat_socket.clone()),
                                regions: Vec::new(),
                            }));

                        match heartbeat_result {
                            Ok(hb) => {
                                if terminal_reported {
                                    info!("Heartbeat accepted again, resuming normal interval.");
                                    terminal_reported = false;
                                }
                                // retry sooner in case a heartbeat fails
                                let new_interval = Duration::from_millis((hb.get_ref().ttl_ms / 3) as u64);
                                if heartbeat_interval != new_interval {
                                    info!("Sending heartbeat every {new_interval:?}.");
                                    heartbeat_interval = new_interval;
                                    heartbeat_tick = crossbeam_channel::tick(new_interval);
                                }
                                successful_heartbeat_count += 1;
                            }
                            Err(err) => {
                                if let Some(retry) = refusal_retry(err.code()) {
                                    if !terminal_reported {
                                        error!("Server refused this destination: {}", err.message());
                                        terminal_reported = true;
                                    }
                                    if heartbeat_interval != retry {
                                        heartbeat_interval = retry;
                                        heartbeat_tick = crossbeam_channel::tick(retry);
                                    }
                                } else {
                                    warn!("Error sending heartbeat: {err}");
                                }
                                datapoint_warn!(
                                    "localshred_lite_proxy-heartbeat_send_error",
                                    "localshred_url" => localshred_url,
                                    ("errors", 1, i64),
                                    ("error_str", err.to_string(), String),
                                );
                                failed_heartbeat_count += 1;
                            }
                        }
                    }

                    // send metrics and handle grpc connection failing
                    recv(metrics_tick) -> _ => {
                        datapoint_info!(
                            "localshred_lite_proxy-heartbeat_stats",
                            "localshred_url" => localshred_url,
                            ("successful_heartbeat_count", successful_heartbeat_count, i64),
                            ("failed_heartbeat_count", failed_heartbeat_count, i64),
                            ("client_restart_count", client_restart_count, i64),
                        );

                        // handle scenario when grpc connection is open, but backend doesn't receive heartbeat
                        // possibly due to envoy losing track of the pod when backend restarts.
                        // we restart our grpc connection to work around the stale connection
                        // if no shreds received, then restart
                        let new_received_count = metrics.agg_received_cumulative.load(Ordering::Relaxed);
                        if new_received_count == last_cumulative_received_shred_count
                            && !terminal_reported
                        {
                            warn!("No shreds received recently, restarting heartbeat client.");
                            datapoint_warn!(
                                "localshred_lite_proxy-heartbeat_restart_signal",
                                "localshred_url" => localshred_url,
                            );
                            break;
                        }
                        last_cumulative_received_shred_count = new_received_count;


                        successful_heartbeat_count_cumulative += successful_heartbeat_count;
                        failed_heartbeat_count_cumulative += failed_heartbeat_count;
                        client_restart_count_cumulative += client_restart_count;
                        successful_heartbeat_count = 0;
                        failed_heartbeat_count = 0;
                        client_restart_count = 0;
                    }

                    // handle SIGINT shutdown
                    recv(shutdown_receiver) -> _ => {
                        // exit should be true
                        break;
                    }
                }
            }
        }
        info!("Exiting heartbeat thread, sent {successful_heartbeat_count_cumulative} successful, {failed_heartbeat_count_cumulative} failed heartbeats. Client restarted {client_restart_count_cumulative} times.");
    }).unwrap()
}

pub async fn get_grpc_client(
    localshred_url: String,
    api_key_header: String,
    api_key: String,
) -> Result<ShredstreamClient<InterceptedService<Channel, ApiKeyInterceptor>>, ShredstreamProxyError>
{
    let channel = create_grpc_channel(localshred_url).await?;
    let api_key_interceptor = ApiKeyInterceptor::new(&api_key_header, &api_key)?;
    Ok(ShredstreamClient::with_interceptor(
        channel,
        api_key_interceptor,
    ))
}
