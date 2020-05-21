use cliff::ExponentialCliffSearcher;
use color_eyre::Report;
use eyre::WrapErr;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    stream::StreamExt,
};
use tracing::instrument;
use tracing_futures::Instrument;
use tsunami::providers::aws;
use tsunami::Tsunami;

/// lobsters-noria; requires two machines: a client and a server
#[instrument(name = "lobsters-noria")]
pub(crate) async fn main() -> Result<(), Report> {
    one(0, 256).await
}

#[instrument]
pub(crate) async fn one(nshards: usize, in_flight: usize) -> Result<(), Report> {
    let mut aws = crate::launcher();
    // aws.set_max_instance_duration(3);

    // try to ensure we do AWS cleanup
    let result: Result<(), Report> = try {
        tracing::info!("spinning up aws instances");
        aws.spawn(
            vec![
                (
                    String::from("server"),
                    aws::Setup::default()
                        .instance_type("r5n.4xlarge")
                        .ami(crate::AMI, "ubuntu")
                        .setup(crate::noria_setup("noria-server", "noria-server")),
                ),
                (
                    String::from("client"),
                    aws::Setup::default()
                        .instance_type("m5n.24xlarge")
                        .ami(crate::AMI, "ubuntu")
                        .setup(crate::noria_setup("noria-applications", "lobsters-noria")),
                ),
            ],
            Some(Duration::from_secs(2 * 60)),
        )
        .await
        .wrap_err("failed to start instances")?;

        tracing::debug!("connecting");
        let vms = aws.connect_all().await?;
        let server = vms.get("server").unwrap();
        let client = vms.get("client").unwrap();
        let s = server.ssh.as_ref().unwrap();
        let c = client.ssh.as_ref().unwrap();
        tracing::debug!("connected");

        let result: Result<(), Report> = try {
            let mut scales = ExponentialCliffSearcher::until(500, 500);
            while let Some(scale) = scales.next() {
                let scale_span = tracing::info_span!("scale", scale);
                async {
                    tracing::info!("start benchmark target");
                    let prefix = format!("lobsters-direct-{}", scale);

                    tracing::trace!("starting noria server");
                    let noria_server = crate::server::build(s, server)
                        .arg("--durability=memory")
                        .arg("--no-reuse")
                        .arg("--shards")
                        .arg(nshards.to_string())
                        .spawn()
                        .wrap_err("failed to start noria-server")?;

                    let mut ready = false;
                    tracing::debug!("prime");
                    let prime = crate::run_with_stderr(
                        lobsters_client(c, server, scale, in_flight)
                            .arg("--warmup=0")
                            .arg("--runtime=0")
                            .arg("--prime"),
                        "prime",
                    )
                    .await
                    .wrap_err("failed to prime")?;
                    tracing::trace!("priming finished");

                    if prime.success() {
                        tracing::trace!("priming succeeded");
                        tracing::debug!("warm");
                        let prime = crate::run_with_stderr(
                            lobsters_client(c, server, scale, in_flight)
                                .arg("--warmup=30")
                                .arg("--runtime=0"),
                            "warm",
                        )
                        .await
                        .wrap_err("failed to warm")?;
                        tracing::trace!("warming finished");
                        ready = prime.success();
                    }

                    if ready {
                        tracing::trace!("warming succeeded");
                        tracing::debug!("benchmark");
                        let mut bench = lobsters_client(c, server, scale, in_flight)
                            .arg("--warmup=40")
                            .arg("--runtime=40")
                            .arg("--histogram=benchmark.hist")
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .spawn()
                            .wrap_err("failed to start client")?;

                        // print errors as we go
                        let mut stderr =
                            tokio::io::BufReader::new(bench.stderr().take().unwrap()).lines();
                        while let Some(line) = stderr.next().await {
                            let line = line.wrap_err("failed to read client output")?;
                            tracing::trace!(message = &*line);
                        }
                        tracing::debug!("benchmark completed");

                        // save normal output
                        tracing::debug!("saving meta-info");
                        tracing::trace!("saving client output");
                        let mut stdout =
                            tokio::io::BufReader::new(bench.stdout().take().unwrap()).lines();
                        let results = tokio::fs::File::create(format!("{}.log", prefix));
                        let results = results.await.wrap_err("failed to create local log file")?;
                        let mut results = tokio::io::BufWriter::new(results);
                        let mut target = None;
                        let mut actual = None;
                        while let Some(line) = stdout.next().await {
                            let line = line.wrap_err("failed to read client output")?;
                            results.write_all(line.as_bytes()).await?;
                            results.write_all(b"\n").await?;

                            if target.is_none() || actual.is_none() {
                                if line.starts_with("# target ops/s") {
                                    target =
                                        Some(line.rsplitn(2, ' ').next().unwrap().parse::<f64>()?);
                                } else if line.starts_with("# generated ops/s") {
                                    actual =
                                        Some(line.rsplitn(2, ' ').next().unwrap().parse::<f64>()?);
                                }
                                if let (Some(target), Some(actual)) = (target, actual) {
                                    if actual < target * 4.0 / 5.0 {
                                        tracing::warn!(%actual, %target, "low throughput");
                                        scales.overloaded();
                                    }
                                }
                            }

                            // Submit          sojourn         95      4484
                            if line.contains("sojourn") {
                                let mut fields = line.trim().split_whitespace();
                                let field = fields.next().unwrap();
                                if let "Login" | "Logout" = field {
                                    // ignore not-that-interesting endpoints
                                    continue;
                                }

                                let metric = if let Some(metric) = fields.next() {
                                    metric
                                } else {
                                    tracing::warn!(case = "bad line", message = &*line);
                                    continue;
                                };
                                if metric != "sojourn" {
                                    assert_eq!(metric, "processing");
                                    continue;
                                }

                                let pct = if let Some(pct) = fields.next() {
                                    pct
                                } else {
                                    tracing::warn!(case = "bad line", message = &*line);
                                    continue;
                                };
                                if pct != "95" {
                                    assert!(pct == "50" || pct == "99" || pct == "100", "{}", pct);
                                    continue;
                                }

                                let ms = if let Some(ms) = fields.next() {
                                    ms
                                } else {
                                    tracing::warn!(case = "bad line", message = &*line);
                                    continue;
                                };
                                let ms: usize = if let Ok(ms) = ms.parse() {
                                    ms
                                } else {
                                    tracing::warn!(case = "bad line", message = &*line);
                                    continue;
                                };
                                if ms > 200 {
                                    tracing::warn!(
                                        endpoint = field,
                                        sojourn = ms,
                                        "high sojourn latency"
                                    );
                                    scales.overloaded();
                                }
                            }
                        }
                        results.flush().await?;

                        if target.is_none() || actual.is_none() {
                            tracing::warn!("missing throughput line, probably overloaded");
                            scales.overloaded();
                        }

                        tracing::trace!("saving load metrics");
                        let (sload1, sload5) =
                            crate::load(s).await.wrap_err("failed to get server load")?;
                        let (cload1, cload5) =
                            crate::load(c).await.wrap_err("failed to get client load")?;
                        results
                            .write_all(format!("# server load: {} {}\n", sload1, sload5).as_bytes())
                            .await?;
                        results
                            .write_all(format!("# client load: {} {}\n", cload1, cload5).as_bytes())
                            .await?;
                        results.flush().await?;
                        drop(results);

                        tracing::trace!("saving histogram");
                        let mut histogram = c
                            .sftp()
                            .read_from("benchmark.hist")
                            .await
                            .wrap_err("failed to read remote histogram")?;
                        let mut results = tokio::fs::File::create(format!("{}.hist", prefix))
                            .await
                            .wrap_err("failed to create local histogram copy")?;
                        tokio::io::copy(&mut histogram, &mut results)
                            .await
                            .wrap_err("failed to save remote histogram")?;
                        drop(results);

                        tracing::trace!("saving server stats");
                        let mut results =
                            tokio::fs::File::create(format!("{}-statistics.json", prefix))
                                .await
                                .wrap_err("failed to create local stats file")?;
                        crate::server::write_stats(s, server, &mut results)
                            .await
                            .wrap_err("failed to save server stats")?;
                        results.flush().await?;
                        drop(results);

                        tracing::debug!("all results saved");

                        let status = bench.wait().await?;

                        if !status.success() {
                            tracing::warn!("client failed");
                            scales.overloaded();
                        }
                    }

                    tracing::debug!("stopping server");
                    crate::server::stop(s, noria_server).await?;
                    tracing::trace!("server stopped");

                    Ok::<_, Report>(())
                }
                .instrument(scale_span)
                .await?;
            }
        };

        tracing::debug!("cleaning up");
        tracing::trace!("cleaning up ssh connections");
        for (name, host) in vms {
            let host_span = tracing::trace_span!("ssh_close", name = &*name);
            let ssh = host.ssh.expect("ssh connection to host disappeared");
            async {
                tracing::trace!("closing connection");
                if let Err(e) = ssh.close().await {
                    tracing::warn!("ssh connection failed: {}", e);
                }
            }
            .instrument(host_span)
            .await
        }

        result?
    };

    tracing::trace!("cleaning up instances");
    let cleanup = aws.terminate_all().await;
    tracing::debug!("done");
    let result = result?;
    let _ = cleanup.wrap_err("cleanup failed")?;
    Ok(result)
}

fn lobsters_client<'c>(
    ssh: &'c tsunami::Session,
    server: &'c tsunami::Machine<'c>,
    scale: usize,
    in_flight: usize,
) -> openssh::Command<'c> {
    let mut cmd = crate::noria_bin(ssh, "noria-applications", "lobsters-noria");
    cmd.arg("--deployment")
        .arg("benchmark")
        .arg("-z")
        .arg(format!(
            "{}:2181",
            server.private_ip.as_ref().expect("private ip unknown")
        ))
        .arg("--scale")
        .arg(scale.to_string())
        .arg("--in-flight")
        .arg(in_flight.to_string());
    cmd
}
