use anyhow::Result;
use clap::{Parser, Subcommand};
use mlat_tools::{extract, replay};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mlat-capture", about = "Capture decoding and replay for mlat-server testing")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Decode a tcpdump capture into a replayable event file (JSON lines)
    Extract {
        /// classic pcap written by `tcpdump -w`
        pcap: PathBuf,
        /// output event file
        #[arg(short, long)]
        out: PathBuf,
        /// server port the clients connect to
        #[arg(long, default_value_t = 31090)]
        port: u16,
    },
    /// Replay an event file against a server under test
    Replay {
        /// event file from `extract`
        input: PathBuf,
        /// host:port of the server under test
        #[arg(long)]
        target: String,
        /// 1.0 = real time, 2.0 = twice as fast
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// prefix added to every replayed receiver's user and uuid
        #[arg(long, default_value = "rp-")]
        prefix: String,
        /// write everything the server sends back to this JSON-lines file
        #[arg(long)]
        results: Option<PathBuf>,
        /// stop after this many seconds of capture time
        #[arg(long)]
        max_secs: Option<f64>,
        /// permit a target that is not loopback/private (never use on production)
        #[arg(long)]
        allow_public: bool,
        /// send sync/mlat messages as UDP datagrams when the server offers the UDP transport
        #[arg(long)]
        udp: bool,
    },
}

fn print_counts(title: &str, m: &std::collections::BTreeMap<String, u64>) {
    println!("{title}:");
    for (k, v) in m {
        println!("  {k:<20} {v}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Extract { pcap, out, port } => {
            let s = extract::run(&pcap, port, &out)?;
            println!("packets read         {}", s.packets);
            println!("tcp packets on port  {}", s.tcp_on_port);
            println!("capture duration     {:.1} s", s.duration);
            println!("connections (SYN)    {}", s.connections);
            println!("  handshakes decoded {}", s.handshakes);
            println!("  denied by server   {}", s.denied);
            println!("  damaged streams    {}", s.damaged);
            println!("streams without SYN  {} (pre-existing connections, not decodable)", s.without_syn);
            print_counts("compression modes", &s.modes);
            print_counts("client -> server messages", &s.c2s_kinds);
            print_counts("server -> client messages", &s.s2c_kinds);
        }
        Cmd::Replay { input, target, speed, prefix, results, max_secs, allow_public, udp } => {
            let r = replay::run(replay::Opts { input, target, speed, prefix, results, max_secs, allow_public, udp }).await?;
            println!("connected            {}", r.connected);
            println!("connect failures     {}", r.connect_failed);
            println!("lines sent           {}", r.lines_sent);
            println!("lines received       {}", r.lines_received);
            println!("send errors          {}", r.send_errors);
            println!("max schedule lag     {:.3} s", r.max_lag_secs);
            println!("replay start epoch   {:.6}", r.start_epoch);
        }
    }
    Ok(())
}
