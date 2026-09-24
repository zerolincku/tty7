//! Bounded, read-only host inventory on a separate SSH channel.
use super::{SshConnection, SshManager};
use russh::ChannelMsg;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub values: BTreeMap<String, String>,
    pub disks: Vec<HostDisk>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDisk {
    pub device: String,
    pub kind: String,
    pub mount: String,
    pub total_kb: u64,
    pub used_kb: u64,
    pub available_kb: u64,
    pub percent: u8,
}

const RESOURCES: &str = r#"
export LC_ALL=C
printf 'PLATFORM=%s\n' "$(uname -s)"
[ "$(uname -s)" = Linux ] || exit 2
# Sum through steal; guest counters are already included in user/nice.
cpu_sample() { awk '/^cpu / {total=0; for(i=2;i<=9;i++) total+=$i; printf "%.0f %.0f", total, $5+$6; exit}' /proc/stat; }
cpu_before=$(cpu_sample)
sleep 0.3
cpu_after=$(cpu_sample)
printf 'CPU_SAMPLE=%s %s\n' "$cpu_before" "$cpu_after"
awk '/^MemTotal:/ {print "MEM_TOTAL=" $2} /^MemAvailable:/ {print "MEM_AVAILABLE=" $2}' /proc/meminfo
if command -v timeout >/dev/null 2>&1; then
 timeout 3 df -lkPT 2>/dev/null | awk 'NR>1 && ($7=="/" || $2 !~ /^(tmpfs|devtmpfs|squashfs|overlay|proc|sysfs)$/) {mount=$7; for(i=8;i<=NF;i++) mount=mount " " $i; printf "DISK\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", $1,$2,$3,$4,$5,$6,mount}'
else
 df -lkPT 2>/dev/null | awk 'NR>1 && ($7=="/" || $2 !~ /^(tmpfs|devtmpfs|squashfs|overlay|proc|sysfs)$/) {mount=$7; for(i=8;i<=NF;i++) mount=mount " " $i; printf "DISK\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", $1,$2,$3,$4,$5,$6,mount}'
fi
"#;
const INVENTORY: &str = r#"
if [ -r /etc/os-release ]; then . /etc/os-release; printf 'OS=%s %s\n' "${NAME% GNU/Linux}" "$VERSION_ID"; fi
printf 'ARCH=%s\nCORES=%s\n' "$(uname -m)" "$(getconf _NPROCESSORS_ONLN)"
probe_tool() {
 tool=$1
 if ! command -v "$tool" >/dev/null 2>&1; then printf '%s_STATE=missing\n' "$tool"; return; fi
 if ! command -v timeout >/dev/null 2>&1; then printf '%s_STATE=unknown\n' "$tool"; return; fi
 version=$(timeout 3 "$tool" --version 2>/dev/null | head -n1)
 printf '%s_VERSION=%s\n' "$tool" "$version"
 if [ -n "$version" ]; then state=installed; else state=unknown; fi
 printf '%s_STATE=%s\n' "$tool" "$state"
}
probe_tool docker
probe_tool rsync
# A kubectl client alone does not establish that this machine is a node.
node_tool=
for tool in k3s rke2 kubelet; do
 if command -v "$tool" >/dev/null 2>&1; then node_tool=$tool; break; fi
done
if [ -n "$node_tool" ]; then
 printf 'KUBE_STATE=installed\nKUBE_DIST=%s\n' "$node_tool"
 if command -v timeout >/dev/null 2>&1; then printf 'KUBE_VERSION=%s\n' "$(timeout 3 "$node_tool" --version 2>/dev/null | head -n1)"; fi
elif [ -d /var/snap/microk8s/current ]; then
 printf 'KUBE_STATE=installed\nKUBE_DIST=MicroK8s\n'
elif [ -e /etc/kubernetes/kubelet.conf ] || [ -e /var/lib/kubelet/config.yaml ] || [ -e /etc/systemd/system/kubelet.service ] || [ -e /usr/lib/systemd/system/kubelet.service ]; then
 printf 'KUBE_STATE=installed\nKUBE_DIST=Kubernetes\n'
else echo KUBE_STATE=undetected; fi
"#;

pub fn collect(conn: &Arc<SshConnection>, full: bool) -> Result<HostInfo, String> {
    SshManager::global().handle().block_on(async {
        tokio::time::timeout(Duration::from_secs(18), async {
            let mut channel = conn
                .open_command_channel()
                .await
                .map_err(|e| e.to_string())?;
            let script = format!(
                "{}\n{}\nprintf 'TTY7_INFO_DONE=1\\n'\n",
                RESOURCES,
                if full { INVENTORY } else { "" }
            );
            channel
                .exec(true, script.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let mut output = Vec::new();
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Data { data } => {
                        if output.len() + data.len() > 64 * 1024 {
                            return Err("host info exceeds output limit".into());
                        }
                        output.extend_from_slice(&data);
                    }
                    ChannelMsg::ExitStatus { exit_status } if exit_status != 0 => {
                        return Err("host information is supported on Linux hosts".into());
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
            parse(&String::from_utf8_lossy(&output))
        })
        .await
        .map_err(|_| "host information collection timed out".to_string())?
    })
}

pub fn parse(output: &str) -> Result<HostInfo, String> {
    let mut info = HostInfo::default();
    for line in output.lines() {
        if let Some(row) = line.strip_prefix("DISK\t") {
            let f: Vec<_> = row.splitn(7, '\t').collect();
            if f.len() != 7 {
                continue;
            }
            if let (Ok(total), Ok(used), Ok(available), Ok(percent)) = (
                f[2].parse(),
                f[3].parse(),
                f[4].parse(),
                f[5].trim_end_matches('%').parse::<u8>(),
            ) {
                if percent <= 100 {
                    info.disks.push(HostDisk {
                        device: f[0].into(),
                        kind: f[1].into(),
                        mount: f[6].into(),
                        total_kb: total,
                        used_kb: used,
                        available_kb: available,
                        percent,
                    });
                }
            }
        } else if let Some((key, value)) = line.split_once('=') {
            if key.len() < 40 && value.len() < 2048 {
                info.values.insert(key.into(), value.into());
            }
        }
    }
    if info.values.get("TTY7_INFO_DONE").map(String::as_str) != Some("1") {
        return Err("incomplete host information".into());
    }
    if let Some(sample) = info.values.remove("CPU_SAMPLE") {
        if let Some(percent) = cpu_percent(&sample) {
            info.values
                .insert("CPU_PERCENT".into(), format!("{percent:.1}"));
        }
    }
    Ok(info)
}

fn cpu_percent(sample: &str) -> Option<f64> {
    let values: Vec<u64> = sample
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() != 4 {
        return None;
    }
    let total = values[2].checked_sub(values[0])?;
    let idle = values[3].checked_sub(values[1])?;
    if total == 0 || idle > total {
        return None;
    }
    Some(100.0 * (total - idle) as f64 / total as f64)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_usage_is_counter_delta_not_load_average() {
        assert_eq!(cpu_percent("1000 800 1200 950"), Some(25.0));
        assert_eq!(cpu_percent("1000 800 1200 800"), Some(100.0));
        assert_eq!(cpu_percent("1000 800 1200 1000"), Some(0.0));
        for sample in [
            "1000 800 1000 800",
            "1000 800 900 700",
            "1000 800 1200 1100",
            "bad",
        ] {
            assert_eq!(cpu_percent(sample), None);
        }
    }
    #[test]
    fn inventory_parses_partial_resources_and_spaced_mounts() {
        let info=parse("MEM_TOTAL=123\nDISK\t/dev/a\text4\t100\t20\t80\t20%\t/data files\nDISK\t/dev/b\txfs\t200\t40\t160\t20%\t/data\nDISK\tbad\nTTY7_INFO_DONE=1\n").unwrap();
        assert_eq!(info.disks.len(), 2);
        assert_eq!(info.disks[1].mount, "/data");
        assert_eq!(info.disks[0].mount, "/data files");
        assert!(!info.values.contains_key("MEM_AVAILABLE"));
        assert!(parse("MEM_TOTAL=123").is_err());
    }
}
