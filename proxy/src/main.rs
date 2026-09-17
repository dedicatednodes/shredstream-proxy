use std::{
    collections::HashMap,
    io,
    io::{Error, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    panic,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread,
    thread::{sleep, spawn, JoinHandle},
    time::Duration,
};

use arc_swap::ArcSwap;
use clap::{arg, Parser};
use crossbeam_channel::{Receiver, RecvError, Sender};
use log::*;
use signal_hook::consts::{SIGINT, SIGTERM};
use solana_client::client_error::{reqwest, ClientError};
use solana_ledger::shred::Shred;
use solana_metrics::set_host_id;
use solana_perf::deduper::Deduper;
use solana_sdk::clock::Slot;
use solana_streamer::streamer::StreamerReceiveStats;
use thiserror::Error;
use tokio::{runtime::Runtime, sync::broadcast::Sender as BroadcastSender};
use tonic::Status;

use crate::{
    api_key_interceptor::{LocalShredConnectionError, DEFAULT_API_KEY_HEADER},
    forwarder::ShredMetrics,
    multicast_config::create_multicast_socket_on_device,
};
mod api_key_interceptor;
mod deshred;
pub mod forwarder;
mod heartbeat;
mod multicast_config;
mod server;

#[derive(Clone, Debug, Parser)]
#[clap(author, version, about, long_about = None)]
// https://docs.rs/clap/latest/clap/_derive/_cookbook/git_derive/index.html
struct Args {
    #[command(subcommand)]
    shredstream_args: ProxySubcommands,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum ProxySubcommands {
    /// Requests shreds from LocalShred Lite and sends to all destinations.
    Shredstream(ShredstreamArgs),

    /// Does not request shreds. Sends anything received on `src-bind-addr`:`src-bind-port` to all destinations.
    ForwardOnly(CommonArgs),
}

#[derive(clap::Args, Clone, Debug)]
struct ShredstreamArgs {
    /// Address of the LocalShred Lite endpoint.
    #[arg(long, env)]
    localshred_url: String,

    /// API key used to authenticate with the endpoint.
    #[arg(long, env)]
    api_key: String,

    /// Header used to send the API key.
    #[arg(long, env, default_value = DEFAULT_API_KEY_HEADER)]
    api_key_header: String,

    #[clap(flatten)]
    common_args: CommonArgs,
}

#[derive(clap::Args, Clone, Debug)]
struct CommonArgs {
    /// Address where LocalShred Lite proxy listens.
    #[arg(long, env, default_value_t = IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)))]
    src_bind_addr: IpAddr,

    /// Port where LocalShred Lite proxy listens. Use `0` for random ephemeral port.
    #[arg(long, env, default_value_t = 20_000)]
    src_bind_port: u16,

    /// Multicast IP to listen for shreds. If none provided, attempts to
    /// parse multicast routes for the device specified by `--multicast-device`
    /// via `ip --json route show dev <device>`.
    #[arg(long, env)]
    multicast_bind_ip: Option<IpAddr>,

    /// Network device to use for multicast route discovery and interface selection.
    /// Example: `eth0`, `en0`, or `doublezero1`.
    #[arg(long, env, default_value = "doublezero1")]
    multicast_device: String,

    /// Port to receive multicast shreds
    #[arg(long, env, default_value_t = 20001)]
    multicast_subscribe_port: u16,

    /// Static set of IP:Port where LocalShred Lite proxy forwards shreds to, comma separated.
    /// Eg. `127.0.0.1:8001,10.0.0.1:8001`.
    // Note: store the original string, so we can do hostname resolution when refreshing destinations
    #[arg(long, env, value_delimiter = ',', value_parser = resolve_hostname_port)]
    dest_ip_ports: Vec<(SocketAddr, String)>,

    /// Http JSON endpoint to dynamically get IPs for LocalShred Lite proxy to forward shreds.
    /// Endpoints are then set-union with `dest-ip-ports`.
    #[arg(long, env)]
    endpoint_discovery_url: Option<String>,

    /// Port to send shreds to for hosts fetched via `endpoint-discovery-url`.
    /// Port can be found using `scripts/get_tvu_port.sh`.
    #[arg(long, env)]
    discovered_endpoints_port: Option<u16>,

    /// Interval between logging stats to stdout and influx
    #[arg(long, env, default_value_t = 15_000)]
    metrics_report_interval_ms: u64,

    /// Logs trace shreds to stdout and influx
    #[arg(long, env, default_value_t = false)]
    debug_trace_shred: bool,

    /// GRPC port for serving decoded shreds as Solana entries
    #[arg(long, env)]
    grpc_service_port: Option<u16>,

    /// Number of threads to use. Defaults to use up to 4.
    #[arg(long, env)]
    num_threads: Option<usize>,
}

#[derive(Debug, Error)]
pub enum ShredstreamProxyError {
    #[error("TonicError {0}")]
    TonicError(#[from] tonic::transport::Error),
    #[error("GrpcError {0}")]
    GrpcError(#[from] Status),
    #[error("ReqwestError {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("SerdeJsonError {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("RpcError {0}")]
    RpcError(#[from] ClientError),
    #[error("LocalShredConnectionError {0}")]
    LocalShredConnectionError(#[from] LocalShredConnectionError),
    #[error("RecvError {0}")]
    RecvError(#[from] RecvError),
    #[error("IoError {0}")]
    IoError(#[from] io::Error),
}

fn resolve_hostname_port(hostname_port: &str) -> io::Result<(SocketAddr, String)> {
    let socketaddr = hostname_port.to_socket_addrs()?.next().ok_or_else(|| {
        Error::new(
            ErrorKind::AddrNotAvailable,
            format!("Could not find destination {hostname_port}"),
        )
    })?;

    Ok((socketaddr, hostname_port.to_string()))
}

// Creates a channel that gets a message every time `SIGINT` is signalled.
fn shutdown_notifier(exit: Arc<AtomicBool>) -> io::Result<(Sender<()>, Receiver<()>)> {
    let (s, r) = crossbeam_channel::bounded(256);
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;

    let s_thread = s.clone();
    thread::spawn(move || {
        for _ in signals.forever() {
            exit.store(true, Ordering::SeqCst);
            // send shutdown signal multiple times since crossbeam doesn't have broadcast channels
            // each thread will consume a shutdown signal
            for _ in 0..256 {
                if s_thread.send(()).is_err() {
                    break;
                }
            }
        }
    });

    Ok((s, r))
}

pub type ReconstructedShredsMap = HashMap<Slot, HashMap<u32 /* fec_set_index */, Vec<Shred>>>;
fn main() -> Result<(), ShredstreamProxyError> {
    env_logger::builder().init();

    let all_args: Args = Args::parse();

    let shredstream_args = all_args.shredstream_args.clone();

    // common args
    let args = match all_args.shredstream_args {
        ProxySubcommands::Shredstream(x) => x.common_args,
        ProxySubcommands::ForwardOnly(x) => x,
    };
    set_host_id(hostname::get()?.into_string().unwrap());
    if (args.endpoint_discovery_url.is_none() && args.discovered_endpoints_port.is_some())
        || (args.endpoint_discovery_url.is_some() && args.discovered_endpoints_port.is_none())
    {
        return Err(ShredstreamProxyError::IoError(io::Error::new(ErrorKind::InvalidInput, "Invalid arguments provided, dynamic endpoints requires both --endpoint-discovery-url and --discovered-endpoints-port.")));
    }
    if args.endpoint_discovery_url.is_none()
        && args.discovered_endpoints_port.is_none()
        && args.dest_ip_ports.is_empty()
    {
        return Err(ShredstreamProxyError::IoError(io::Error::new(ErrorKind::InvalidInput, "No destinations found. You must provide values for --dest-ip-ports or --endpoint-discovery-url.")));
    }

    let exit = Arc::new(AtomicBool::new(false));
    let (shutdown_sender, shutdown_receiver) =
        shutdown_notifier(exit.clone()).expect("Failed to set up signal handler");
    let panic_hook = panic::take_hook();
    {
        let exit = exit.clone();
        panic::set_hook(Box::new(move |panic_info| {
            exit.store(true, Ordering::SeqCst);
            let _ = shutdown_sender.send(());
            error!("exiting process");
            sleep(Duration::from_secs(1));
            // invoke the default handler and exit the process
            panic_hook(panic_info);
        }));
    }

    let metrics = Arc::new(ShredMetrics::new(args.grpc_service_port.is_some()));

    let runtime = Runtime::new()?;
    let mut thread_handles = vec![];
    if let ProxySubcommands::Shredstream(args) = shredstream_args {
        let heartbeat_hdl =
            start_heartbeat(args, &exit, &shutdown_receiver, runtime, metrics.clone());
        thread_handles.push(heartbeat_hdl);
    }

    // share sockets between refresh and forwarder thread
    let unioned_dest_sockets = Arc::new(ArcSwap::from_pointee(
        args.dest_ip_ports
            .iter()
            .map(|x| x.0)
            .collect::<Vec<SocketAddr>>(),
    ));

    // share deduper + metrics between forwarder <-> accessory thread
    // use mutex since metrics are write heavy. cheaper than rwlock
    let deduper = Arc::new(RwLock::new(Deduper::<2, [u8]>::new(
        &mut rand::thread_rng(),
        forwarder::DEDUPER_NUM_BITS,
    )));

    let entry_sender = Arc::new(BroadcastSender::new(100));
    let forward_stats = Arc::new(StreamerReceiveStats::new("localshred_lite_proxy-listen_thread"));
    let use_discovery_service =
        args.endpoint_discovery_url.is_some() && args.discovered_endpoints_port.is_some();
    let maybe_multicast_socket = create_multicast_socket_on_device(
        &args.multicast_device,
        args.multicast_subscribe_port,
        args.multicast_bind_ip,
    )
    .inspect(|mcast_socket| info!("Multicast listeners found: {mcast_socket:?}."));
    let forwarder_hdls = forwarder::start_forwarder_threads(
        unioned_dest_sockets.clone(),
        args.src_bind_addr,
        args.src_bind_port,
        maybe_multicast_socket,
        args.num_threads,
        deduper.clone(),
        args.grpc_service_port.is_some(),
        entry_sender.clone(),
        args.debug_trace_shred,
        use_discovery_service,
        forward_stats.clone(),
        metrics.clone(),
        shutdown_receiver.clone(),
        exit.clone(),
    );
    thread_handles.extend(forwarder_hdls);

    let report_metrics_thread = {
        let exit = exit.clone();
        spawn(move || {
            while !exit.load(Ordering::Relaxed) {
                sleep(Duration::from_secs(1));
                forward_stats.report();
            }
        })
    };
    thread_handles.push(report_metrics_thread);

    let metrics_hdl = forwarder::start_forwarder_accessory_thread(
        deduper,
        metrics.clone(),
        args.metrics_report_interval_ms,
        shutdown_receiver.clone(),
        exit.clone(),
    );
    thread_handles.push(metrics_hdl);
    if use_discovery_service {
        let refresh_handle = forwarder::start_destination_refresh_thread(
            args.endpoint_discovery_url.unwrap(),
            args.discovered_endpoints_port.unwrap(),
            args.dest_ip_ports,
            unioned_dest_sockets,
            shutdown_receiver.clone(),
            exit.clone(),
        );
        thread_handles.push(refresh_handle);
    }

    if let Some(port) = args.grpc_service_port {
        let server_hdl = server::start_server_thread(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            entry_sender.clone(),
            exit.clone(),
            shutdown_receiver.clone(),
        );
        thread_handles.push(server_hdl);
    }

    info!(
        "LocalShred Lite proxy started, listening on {}:{}/udp.",
        args.src_bind_addr, args.src_bind_port
    );

    for thread in thread_handles {
        thread.join().expect("thread panicked");
    }

    info!(
        "Exiting LocalShred Lite proxy, {} received, {} sent successfully, {} failed, {} duplicate shreds.",
        metrics.agg_received_cumulative.load(Ordering::Relaxed),
        metrics
            .agg_success_forward_cumulative
            .load(Ordering::Relaxed),
        metrics.agg_fail_forward_cumulative.load(Ordering::Relaxed),
        metrics.duplicate_cumulative.load(Ordering::Relaxed),
    );
    Ok(())
}

fn start_heartbeat(
    args: ShredstreamArgs,
    exit: &Arc<AtomicBool>,
    shutdown_receiver: &Receiver<()>,
    runtime: Runtime,
    metrics: Arc<ShredMetrics>,
) -> JoinHandle<()> {
    heartbeat::heartbeat_loop_thread(
        args.localshred_url,
        args.api_key_header,
        args.api_key,
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            args.common_args.src_bind_port,
        ),
        runtime,
        metrics,
        shutdown_receiver.clone(),
        exit.clone(),
    )
}
