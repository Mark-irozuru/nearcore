use crate::{metrics, SyncStatus};
use actix::Addr;
use near_chain_configs::{ClientConfig, LogSummaryStyle};
use near_client_primitives::types::ShardSyncStatus;
use near_metrics::{try_create_gauge_vec, try_create_int_gauge};
use near_network::types::NetworkInfo;
use near_primitives::block::Tip;
use near_primitives::network::PeerId;
use near_primitives::serialize::to_base;
use near_primitives::telemetry::{
    TelemetryAgentInfo, TelemetryChainInfo, TelemetryInfo, TelemetrySystemInfo,
};
use near_primitives::time::{Clock, Instant};
use near_primitives::types::{AccountId, BlockHeight, EpochHeight, Gas, NumBlocks, ShardId};
use near_primitives::validator_signer::ValidatorSigner;
use near_primitives::version::{Version, DB_VERSION, PROTOCOL_VERSION};
use near_primitives::views::{CurrentEpochValidatorInfo, EpochValidatorInfo, ValidatorKickoutView};
use near_telemetry::{telemetry, TelemetryActor};
use prometheus::{GaugeVec, IntGauge};
use std::cmp::min;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use sysinfo::{get_current_pid, set_open_files_limit, Pid, ProcessExt, System, SystemExt};
use tracing::{info, warn};

const TERAGAS: f64 = 1_000_000_000_000_f64;

pub struct ValidatorInfoHelper {
    pub is_validator: bool,
    pub num_validators: usize,
}

/// A helper that prints information about current chain and reports to telemetry.
pub struct InfoHelper {
    /// Nearcore agent (executable) version
    nearcore_version: Version,
    /// System reference.
    sys: System,
    /// Process id to query resources.
    pid: Option<Pid>,
    /// Timestamp when client was started.
    started: Instant,
    /// Total number of blocks processed.
    num_blocks_processed: u64,
    /// Total number of blocks processed.
    num_chunks_in_blocks_processed: u64,
    /// Total gas used during period.
    gas_used: u64,
    /// Sign telemetry with block producer key if available.
    validator_signer: Option<Arc<dyn ValidatorSigner>>,
    /// Telemetry actor.
    telemetry_actor: Addr<TelemetryActor>,
    /// Log coloring enabled
    log_summary_style: LogSummaryStyle,
    /// Wrapper for re-exporting RocksDB stats into Prometheus metrics.
    rocksdb_metrics: RocksDBMetrics,
}

impl InfoHelper {
    pub fn new(
        telemetry_actor: Addr<TelemetryActor>,
        client_config: &ClientConfig,
        validator_signer: Option<Arc<dyn ValidatorSigner>>,
    ) -> Self {
        set_open_files_limit(0);
        InfoHelper {
            nearcore_version: client_config.version.clone(),
            sys: System::new(),
            pid: get_current_pid().ok(),
            started: Clock::instant(),
            num_blocks_processed: 0,
            num_chunks_in_blocks_processed: 0,
            gas_used: 0,
            telemetry_actor,
            validator_signer,
            log_summary_style: client_config.log_summary_style,
            rocksdb_metrics: RocksDBMetrics::default(),
        }
    }

    pub fn chunk_processed(&mut self, shard_id: ShardId, gas_used: Gas) {
        metrics::TGAS_USAGE_HIST
            .with_label_values(&[&format!("{}", shard_id)])
            .observe(gas_used as f64 / TERAGAS);
    }

    pub fn chunk_skipped(&mut self, shard_id: ShardId) {
        metrics::CHUNK_SKIPPED_TOTAL.with_label_values(&[&format!("{}", shard_id)]).inc();
    }

    pub fn block_processed(&mut self, gas_used: Gas, num_chunks: u64) {
        self.num_blocks_processed += 1;
        self.num_chunks_in_blocks_processed += num_chunks;
        self.gas_used += gas_used;
    }

    pub fn info(
        &mut self,
        genesis_height: BlockHeight,
        head: &Tip,
        sync_status: &SyncStatus,
        node_id: &PeerId,
        network_info: &NetworkInfo,
        validator_info: Option<ValidatorInfoHelper>,
        validator_epoch_stats: Vec<ValidatorProductionStats>,
        epoch_height: EpochHeight,
        protocol_upgrade_block_height: BlockHeight,
        statistics: Option<String>,
    ) {
        let use_colour = matches!(self.log_summary_style, LogSummaryStyle::Colored);
        let paint = |colour: ansi_term::Colour, text: Option<String>| match text {
            None => ansi_term::Style::default().paint(""),
            Some(text) if use_colour => colour.bold().paint(text),
            Some(text) => ansi_term::Style::default().paint(text),
        };

        let s = |num| if num == 1 { "" } else { "s" };

        let sync_status_log = Some(display_sync_status(sync_status, head, genesis_height));

        let validator_info_log = validator_info.as_ref().map(|info| {
            format!(
                " {}{} validator{}",
                if info.is_validator { "Validator | " } else { "" },
                info.num_validators,
                s(info.num_validators)
            )
        });

        let network_info_log = Some(format!(
            " {} peer{} ⬇ {} ⬆ {}",
            network_info.num_connected_peers,
            s(network_info.num_connected_peers),
            pretty_bytes_per_sec(network_info.received_bytes_per_sec),
            pretty_bytes_per_sec(network_info.sent_bytes_per_sec)
        ));

        let avg_bls = (self.num_blocks_processed as f64)
            / (self.started.elapsed().as_millis() as f64)
            * 1000.0;
        let chunks_per_block = if self.num_blocks_processed > 0 {
            (self.num_chunks_in_blocks_processed as f64) / (self.num_blocks_processed as f64)
        } else {
            0.
        };
        let avg_gas_used =
            ((self.gas_used as f64) / (self.started.elapsed().as_millis() as f64) * 1000.0) as u64;
        let blocks_info_log =
            Some(format!(" {:.2} bps {}", avg_bls, gas_used_per_sec(avg_gas_used)));

        let proc_info = self.pid.filter(|pid| self.sys.refresh_process(*pid)).map(|pid| {
            let proc = self
                .sys
                .get_process(pid)
                .expect("refresh_process succeeds, this should be not None");
            (proc.cpu_usage(), proc.memory())
        });
        let machine_info_log = proc_info
            .as_ref()
            .map(|(cpu, mem)| format!(" CPU: {:.0}%, Mem: {}", cpu, pretty_bytes(mem * 1024)));

        info!(
            target: "stats", "{}{}{}{}{}",
            paint(ansi_term::Colour::Yellow, sync_status_log),
            paint(ansi_term::Colour::White, validator_info_log),
            paint(ansi_term::Colour::Cyan, network_info_log),
            paint(ansi_term::Colour::Green, blocks_info_log),
            paint(ansi_term::Colour::Blue, machine_info_log),
        );
        self.export_rocksdb_statistics(statistics);

        let (cpu_usage, memory_usage) = proc_info.unwrap_or_default();
        let is_validator = validator_info.map(|v| v.is_validator).unwrap_or_default();
        (metrics::IS_VALIDATOR.set(is_validator as i64));
        (metrics::RECEIVED_BYTES_PER_SECOND.set(network_info.received_bytes_per_sec as i64));
        (metrics::SENT_BYTES_PER_SECOND.set(network_info.sent_bytes_per_sec as i64));
        (metrics::BLOCKS_PER_MINUTE.set((avg_bls * (60 as f64)) as i64));
        (metrics::CHUNKS_PER_BLOCK_MILLIS.set((1000. * chunks_per_block) as i64));
        (metrics::CPU_USAGE.set(cpu_usage as i64));
        (metrics::MEMORY_USAGE.set((memory_usage * 1024) as i64));
        (metrics::AVG_TGAS_USAGE.set((avg_gas_used as f64 / TERAGAS).round() as i64));
        (metrics::EPOCH_HEIGHT.set(epoch_height as i64));
        (metrics::PROTOCOL_UPGRADE_BLOCK_HEIGHT.set(protocol_upgrade_block_height as i64));
        (metrics::NODE_PROTOCOL_VERSION.set(PROTOCOL_VERSION as i64));
        (metrics::NODE_DB_VERSION.set(DB_VERSION as i64));

        // In case we can't get the list of validators for the current and the previous epoch,
        // skip updating the per-validator metrics.
        // Note that the metrics are set to 0 for previous epoch validators who are no longer
        // validators.
        for stats in validator_epoch_stats {
            (metrics::VALIDATORS_BLOCKS_PRODUCED
                .with_label_values(&[stats.account_id.as_str()])
                .set(stats.num_produced_blocks as i64));
            (metrics::VALIDATORS_BLOCKS_EXPECTED
                .with_label_values(&[stats.account_id.as_str()])
                .set(stats.num_expected_blocks as i64));
            (metrics::VALIDATORS_CHUNKS_PRODUCED
                .with_label_values(&[stats.account_id.as_str()])
                .set(stats.num_produced_chunks as i64));
            (metrics::VALIDATORS_CHUNKS_EXPECTED
                .with_label_values(&[stats.account_id.as_str()])
                .set(stats.num_expected_chunks as i64));
        }

        self.started = Clock::instant();
        self.num_blocks_processed = 0;
        self.num_chunks_in_blocks_processed = 0;
        self.gas_used = 0;

        let info = TelemetryInfo {
            agent: TelemetryAgentInfo {
                name: "near-rs".to_string(),
                version: self.nearcore_version.version.clone(),
                build: self.nearcore_version.build.clone(),
            },
            system: TelemetrySystemInfo {
                bandwidth_download: network_info.received_bytes_per_sec,
                bandwidth_upload: network_info.sent_bytes_per_sec,
                cpu_usage,
                memory_usage,
            },
            chain: TelemetryChainInfo {
                node_id: node_id.to_string(),
                account_id: self.validator_signer.as_ref().map(|bp| bp.validator_id().clone()),
                is_validator,
                status: sync_status.as_variant_name().to_string(),
                latest_block_hash: to_base(&head.last_block_hash),
                latest_block_height: head.height,
                num_peers: network_info.num_connected_peers,
            },
        };
        // Sign telemetry if there is a signer present.
        let content = if let Some(vs) = self.validator_signer.as_ref() {
            vs.sign_telemetry(&info)
        } else {
            serde_json::to_value(&info).expect("Telemetry must serialize to json")
        };
        telemetry(&self.telemetry_actor, content);
    }

    fn export_stats_as_metrics(&mut self, stats: &[(&str, Vec<StatsValue>)]) {
        for (stats_name, values) in stats {
            if values.len() == 1 {
                // A counter stats.
                if let StatsValue::Count(value) = values[0] {
                    let entry = self.rocksdb_metrics.int_gauges.entry(stats_name.to_string());
                    entry
                        .or_insert_with(|| {
                            try_create_int_gauge(
                                &get_prometheus_metric_name(stats_name),
                                stats_name,
                            )
                            .unwrap()
                        })
                        .set(value);
                }
            } else {
                // A summary stats.
                for stats_value in values {
                    match stats_value {
                        StatsValue::Count(value) => {
                            let entry = self
                                .rocksdb_metrics
                                .int_gauges
                                .entry(get_stats_summary_count_key(stats_name));
                            entry
                                .or_insert_with(|| {
                                    try_create_int_gauge(
                                        &get_metric_name_summary_count_gauge(stats_name),
                                        stats_name,
                                    )
                                    .unwrap()
                                })
                                .set(*value);
                        }
                        StatsValue::Sum(value) => {
                            let entry = self
                                .rocksdb_metrics
                                .int_gauges
                                .entry(get_stats_summary_sum_key(stats_name));
                            entry
                                .or_insert_with(|| {
                                    try_create_int_gauge(
                                        &get_metric_name_summary_sum_gauge(stats_name),
                                        stats_name,
                                    )
                                    .unwrap()
                                })
                                .set(*value);
                        }
                        StatsValue::Percentile(percentile, value) => {
                            let entry = self.rocksdb_metrics.gauges.entry(stats_name.to_string());
                            entry
                                .or_insert_with(|| {
                                    try_create_gauge_vec(
                                        &get_prometheus_metric_name(stats_name),
                                        stats_name,
                                        &["quantile"],
                                    )
                                    .unwrap()
                                })
                                .with_label_values(&[&format!("{:.2}", *percentile as f64 * 0.01)])
                                .set(*value);
                        }
                    }
                }
            }
        }
    }

    fn export_rocksdb_statistics(&mut self, statistics: Option<String>) {
        if let Some(statistics) = statistics {
            match parse_statistics(&statistics) {
                Ok(stats) => {
                    self.export_stats_as_metrics(&stats);
                }
                Err(err) => {
                    warn!(target: "stats", "Failed to parse rocksdb statistics: {:?}", err);
                }
            }
        }
    }
}

#[derive(Default)]
struct RocksDBMetrics {
    int_gauges: HashMap<String, IntGauge>,
    gauges: HashMap<String, GaugeVec>,
}

#[derive(Debug, Clone, Copy)]
enum StatsValue {
    Count(i64),
    Sum(i64),
    Percentile(u32, f64),
}

fn get_prometheus_metric_name(stats_name: &str) -> String {
    format!("near_{}", stats_name.replace(".", "_"))
}

fn get_metric_name_summary_count_gauge(stats_name: &str) -> String {
    format!("near_{}_count", stats_name.replace(".", "_"))
}

fn get_metric_name_summary_sum_gauge(stats_name: &str) -> String {
    format!("near_{}_sum", stats_name.replace(".", "_"))
}

fn get_stats_summary_count_key(stats_name: &str) -> String {
    format!("{}.count", stats_name)
}

fn get_stats_summary_sum_key(stats_name: &str) -> String {
    format!("{}.sum", stats_name)
}

fn parse_statistics(statistics: &str) -> Result<Vec<(&str, Vec<StatsValue>)>, anyhow::Error> {
    let mut result = vec![];
    for line in statistics.split('\n') {
        let mut values = vec![];
        let words: Vec<&str> = line.split(' ').collect();
        if words.len() > 1 {
            let stats_name = words[0];
            for i in (1..words.len()).step_by(3) {
                if words[i] == "COUNT" {
                    values.push(StatsValue::Count(
                        words[i + 2].parse::<i64>().map_err(|err| anyhow::anyhow!(err))?,
                    ));
                } else if words[i] == "SUM" {
                    values.push(StatsValue::Sum(
                        words[i + 2].parse::<i64>().map_err(|err| anyhow::anyhow!(err))?,
                    ));
                } else if words[i].starts_with("P") {
                    values.push(StatsValue::Percentile(
                        words[i][1..].parse::<u32>().map_err(|err| anyhow::anyhow!(err))?,
                        words[i + 2].parse::<f64>().map_err(|err| anyhow::anyhow!(err))?,
                    ));
                } else {
                    return Err(anyhow::anyhow!(
                        "Unsupported stats value: {} in {}",
                        words[i],
                        line
                    ));
                }
            }
            result.push((stats_name, values));
        }
    }
    Ok(result)
}

fn display_sync_status(
    sync_status: &SyncStatus,
    head: &Tip,
    genesis_height: BlockHeight,
) -> String {
    metrics::SYNC_STATUS.set(sync_status.repr() as i64);
    match sync_status {
        SyncStatus::AwaitingPeers => format!("#{:>8} Waiting for peers", head.height),
        SyncStatus::NoSync => format!("#{:>8} {:>44}", head.height, head.last_block_hash),
        SyncStatus::EpochSync { epoch_ord } => {
            format!("[EPOCH: {:>5}] Getting to a recent epoch", epoch_ord)
        }
        SyncStatus::HeaderSync { current_height, highest_height } => {
            let percent = if *highest_height <= genesis_height {
                0.0
            } else {
                (((min(current_height, highest_height) - genesis_height) * 100) as f64)
                    / ((highest_height - genesis_height) as f64)
            };
            format!(
                "#{:>8} Downloading headers {:.2}% ({})",
                head.height,
                percent,
                highest_height - current_height
            )
        }
        SyncStatus::BodySync { current_height, highest_height } => {
            let percent = if *highest_height <= genesis_height {
                0.0
            } else {
                ((current_height - genesis_height) * 100) as f64
                    / ((highest_height - genesis_height) as f64)
            };
            format!(
                "#{:>8} Downloading blocks {:.2}% ({})",
                head.height,
                percent,
                highest_height - current_height
            )
        }
        SyncStatus::StateSync(sync_hash, shard_statuses) => {
            let mut res = format!("State {:?}", sync_hash);
            let mut shard_statuses: Vec<_> = shard_statuses.iter().collect();
            shard_statuses.sort_by_key(|(shard_id, _)| *shard_id);
            for (shard_id, shard_status) in shard_statuses {
                write!(
                    res,
                    "[{}: {}]",
                    shard_id,
                    match shard_status.status {
                        ShardSyncStatus::StateDownloadHeader => "header",
                        ShardSyncStatus::StateDownloadParts => "parts",
                        ShardSyncStatus::StateDownloadScheduling => "scheduling",
                        ShardSyncStatus::StateDownloadApplying => "applying",
                        ShardSyncStatus::StateDownloadComplete => "download complete",
                        ShardSyncStatus::StateSplitScheduling => "split scheduling",
                        ShardSyncStatus::StateSplitApplying => "split applying",
                        ShardSyncStatus::StateSyncDone => "done",
                    }
                )
                .unwrap();
            }
            res
        }
        SyncStatus::StateSyncDone => format!("State sync done"),
    }
}

const KILOBYTE: u64 = 1024;
const MEGABYTE: u64 = KILOBYTE * 1024;
const GIGABYTE: u64 = MEGABYTE * 1024;

/// Format bytes per second in a nice way.
fn pretty_bytes_per_sec(num: u64) -> String {
    if num < 100 {
        // Under 0.1 kiB, display in bytes.
        format!("{} B/s", num)
    } else if num < MEGABYTE {
        // Under 1.0 MiB/sec display in kiB/sec.
        format!("{:.1}kiB/s", num as f64 / KILOBYTE as f64)
    } else {
        format!("{:.1}MiB/s", num as f64 / MEGABYTE as f64)
    }
}

fn pretty_bytes(num: u64) -> String {
    if num < 1024 {
        format!("{} B", num)
    } else if num < MEGABYTE {
        format!("{:.1} kiB", num as f64 / KILOBYTE as f64)
    } else if num < GIGABYTE {
        format!("{:.1} MiB", num as f64 / MEGABYTE as f64)
    } else {
        format!("{:.1} GiB", num as f64 / GIGABYTE as f64)
    }
}

fn gas_used_per_sec(num: u64) -> String {
    if num < 1000 {
        format!("{} gas/s", num)
    } else if num < 1_000_000 {
        format!("{:.2} Kgas/s", num as f64 / 1_000.0)
    } else if num < 1_000_000_000 {
        format!("{:.2} Mgas/s", num as f64 / 1_000_000.0)
    } else if num < 1_000_000_000_000 {
        format!("{:.2} Ggas/s", num as f64 / 1_000_000_000.0)
    } else {
        format!("{:.2} Tgas/s", num as f64 / 1_000_000_000_000.0)
    }
}

/// Number of blocks and chunks produced and expected by a certain validator.
pub struct ValidatorProductionStats {
    pub account_id: AccountId,
    pub num_produced_blocks: NumBlocks,
    pub num_expected_blocks: NumBlocks,
    pub num_produced_chunks: NumBlocks,
    pub num_expected_chunks: NumBlocks,
}

impl ValidatorProductionStats {
    pub fn kickout(kickout: ValidatorKickoutView) -> Self {
        Self {
            account_id: kickout.account_id,
            num_produced_blocks: 0,
            num_expected_blocks: 0,
            num_produced_chunks: 0,
            num_expected_chunks: 0,
        }
    }
    pub fn validator(info: CurrentEpochValidatorInfo) -> Self {
        Self {
            account_id: info.account_id,
            num_produced_blocks: info.num_produced_blocks,
            num_expected_blocks: info.num_expected_blocks,
            num_produced_chunks: info.num_produced_chunks,
            num_expected_chunks: info.num_expected_chunks,
        }
    }
}

/// Converts EpochValidatorInfo into a vector of ValidatorProductionStats.
pub fn get_validator_epoch_stats(
    current_validator_epoch_info: EpochValidatorInfo,
) -> Vec<ValidatorProductionStats> {
    let mut stats = vec![];
    // Record kickouts to replace latest stats of kicked out validators with zeros.
    for kickout in current_validator_epoch_info.prev_epoch_kickout {
        stats.push(ValidatorProductionStats::kickout(kickout));
    }
    for validator in current_validator_epoch_info.current_validators {
        stats.push(ValidatorProductionStats::validator(validator));
    }
    stats
}
