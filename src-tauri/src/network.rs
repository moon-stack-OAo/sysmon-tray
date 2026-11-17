use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "interface", rename_all = "camelCase")]
pub enum NetworkSelection {
    #[default]
    Auto,
    All,
    Interface(String),
}

impl NetworkSelection {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Interface(name) = self {
            if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
                return Err("网卡名称无效".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    pub name: String,
    pub virtual_interface: bool,
}

pub struct InterfaceCounter {
    pub name: String,
    pub received: u64,
    pub transmitted: u64,
    pub has_address: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct NetworkRate {
    pub down_bps: u64,
    pub up_bps: u64,
    pub interface: Option<String>,
    pub available: bool,
}

#[derive(Default)]
pub struct NetworkSampler {
    previous: HashMap<String, (u64, u64)>,
    selection: Option<NetworkSelection>,
    selected: Option<String>,
    challenger: Option<(String, u8)>,
}

fn is_virtual(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "loopback",
        "vethernet",
        "virtual",
        "vpn",
        "tap",
        "tun",
        "docker",
        "wsl",
        "hyper-v",
        "vmware",
        "zerotier",
        "tailscale",
        "wireguard",
        "npcap",
    ]
    .iter()
    .any(|pattern| name.contains(pattern))
}

impl NetworkSampler {
    pub fn interfaces(counters: &[InterfaceCounter]) -> Vec<NetworkInterface> {
        let mut interfaces: Vec<_> = counters
            .iter()
            .map(|counter| NetworkInterface {
                name: counter.name.clone(),
                virtual_interface: is_virtual(&counter.name),
            })
            .collect();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));
        interfaces
    }

    pub fn sample(
        &mut self,
        counters: &[InterfaceCounter],
        selection: &NetworkSelection,
        elapsed: Duration,
    ) -> NetworkRate {
        let continuous = !elapsed.is_zero() && elapsed <= Duration::from_secs(5);
        let rates: HashMap<_, _> = counters
            .iter()
            .map(|counter| {
                let delta = self
                    .previous
                    .get(&counter.name)
                    .filter(|(down, up)| {
                        continuous && counter.received >= *down && counter.transmitted >= *up
                    })
                    .map(|(down, up)| {
                        let secs = elapsed.as_secs_f64();
                        (
                            ((counter.received - down) as f64 / secs) as u64,
                            ((counter.transmitted - up) as f64 / secs) as u64,
                        )
                    })
                    .unwrap_or((0, 0));
                (counter.name.clone(), delta)
            })
            .collect();
        self.previous = counters
            .iter()
            .map(|counter| {
                (
                    counter.name.clone(),
                    (counter.received, counter.transmitted),
                )
            })
            .collect();

        let selection_changed = self.selection.as_ref() != Some(selection);
        if selection_changed || !continuous {
            self.challenger = None;
        }
        let selected = match selection {
            NetworkSelection::All => Some("全部接口（汇总）".to_string()),
            NetworkSelection::Interface(name) => Some(name.clone()),
            NetworkSelection::Auto => {
                let physical: Vec<_> = counters
                    .iter()
                    .filter(|counter| !is_virtual(&counter.name))
                    .collect();
                let pool = if physical.is_empty() {
                    counters.iter().collect::<Vec<_>>()
                } else {
                    physical
                };
                let addressed: Vec<_> = pool
                    .iter()
                    .copied()
                    .filter(|counter| counter.has_address)
                    .collect();
                let pool = if addressed.is_empty() {
                    pool
                } else {
                    addressed
                };
                let traffic = |name: &str| {
                    rates
                        .get(name)
                        .map(|(down, up)| down.saturating_add(*up))
                        .unwrap_or(0)
                };
                let best = pool
                    .iter()
                    .max_by(|a, b| {
                        traffic(&a.name)
                            .cmp(&traffic(&b.name))
                            .then_with(|| b.name.cmp(&a.name))
                    })
                    .map(|counter| counter.name.clone());
                let current = self.selected.as_ref().filter(|name| {
                    !selection_changed && pool.iter().any(|counter| &counter.name == *name)
                });
                match (current, best) {
                    (Some(current), Some(best))
                        if current != &best
                            && traffic(&best) > traffic(current).saturating_mul(4).max(1024) =>
                    {
                        let count = self
                            .challenger
                            .as_ref()
                            .filter(|(name, _)| name == &best)
                            .map(|(_, count)| count + 1)
                            .unwrap_or(1);
                        self.challenger = Some((best.clone(), count));
                        if count >= 3 {
                            self.challenger = None;
                            Some(best)
                        } else {
                            Some(current.clone())
                        }
                    }
                    (Some(current), _) => {
                        self.challenger = None;
                        Some(current.clone())
                    }
                    (_, best) => {
                        self.challenger = None;
                        best
                    }
                }
            }
        };
        let switched = selection_changed || self.selected != selected;
        self.selection = Some(selection.clone());
        self.selected = selected.clone();
        let (down_bps, up_bps, available) = match selection {
            NetworkSelection::All => {
                let (down, up) =
                    rates
                        .values()
                        .fold((0u64, 0u64), |(down, up), (next_down, next_up)| {
                            (down.saturating_add(*next_down), up.saturating_add(*next_up))
                        });
                (down, up, !counters.is_empty())
            }
            _ => selected
                .as_ref()
                .and_then(|name| rates.get(name))
                .map(|(down, up)| (*down, *up, true))
                .unwrap_or((0, 0, false)),
        };
        NetworkRate {
            down_bps: if switched { 0 } else { down_bps },
            up_bps: if switched { 0 } else { up_bps },
            interface: selected,
            available,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn counter(name: &str, received: u64) -> InterfaceCounter {
        InterfaceCounter {
            name: name.to_string(),
            received,
            transmitted: received / 2,
            has_address: true,
        }
    }
    #[test]
    fn first_sample_and_new_interface_establish_independent_baselines() {
        let mut sampler = NetworkSampler::default();
        let mode = NetworkSelection::All;
        assert_eq!(
            sampler
                .sample(
                    &[counter("Ethernet", 100000)],
                    &mode,
                    Duration::from_secs(1)
                )
                .down_bps,
            0
        );
        assert_eq!(
            sampler
                .sample(
                    &[counter("Ethernet", 101000), counter("Wi-Fi", 900000)],
                    &mode,
                    Duration::from_secs(1)
                )
                .down_bps,
            1000
        );
        assert_eq!(
            sampler
                .sample(&[counter("Wi-Fi", 901000)], &mode, Duration::from_secs(1))
                .down_bps,
            1000
        );
    }
    #[test]
    fn auto_uses_single_physical_interface_with_matching_name_and_stable_switch() {
        let mut sampler = NetworkSampler::default();
        let mode = NetworkSelection::Auto;
        sampler.sample(
            &[
                counter("Ethernet", 0),
                counter("Wi-Fi", 0),
                counter("VPN", 0),
            ],
            &mode,
            Duration::from_secs(1),
        );
        let frame = sampler.sample(
            &[
                counter("Ethernet", 1000),
                counter("Wi-Fi", 20000),
                counter("VPN", 90000),
            ],
            &mode,
            Duration::from_secs(1),
        );
        assert_eq!(frame.interface.as_deref(), Some("Ethernet"));
        assert_eq!(frame.down_bps, 1000);
        sampler.sample(
            &[counter("Ethernet", 2000), counter("Wi-Fi", 40000)],
            &mode,
            Duration::from_secs(1),
        );
        let frame = sampler.sample(
            &[counter("Ethernet", 3000), counter("Wi-Fi", 60000)],
            &mode,
            Duration::from_secs(1),
        );
        assert_eq!(frame.interface.as_deref(), Some("Wi-Fi"));
        assert_eq!(frame.down_bps, 0);
        assert_eq!(
            sampler
                .sample(&[counter("Wi-Fi", 80000)], &mode, Duration::from_secs(1))
                .down_bps,
            20000
        );
    }
    #[test]
    fn reset_suspend_and_selection_change_do_not_spike() {
        let mut sampler = NetworkSampler::default();
        let mode = NetworkSelection::Interface("Ethernet".into());
        sampler.sample(&[counter("Ethernet", 1000)], &mode, Duration::from_secs(1));
        assert_eq!(
            sampler
                .sample(&[counter("Ethernet", 1)], &mode, Duration::from_secs(1))
                .down_bps,
            0
        );
        assert_eq!(
            sampler
                .sample(
                    &[counter("Ethernet", 100000)],
                    &mode,
                    Duration::from_secs(50)
                )
                .down_bps,
            0
        );
        assert_eq!(
            sampler
                .sample(
                    &[counter("Ethernet", 101000)],
                    &mode,
                    Duration::from_secs(1)
                )
                .down_bps,
            1000
        );
        assert_eq!(
            sampler
                .sample(
                    &[counter("Ethernet", 102000)],
                    &NetworkSelection::All,
                    Duration::from_secs(1)
                )
                .down_bps,
            0
        );
    }
    #[test]
    fn missing_selected_interface_does_not_silently_fall_back() {
        let mut sampler = NetworkSampler::default();
        let mode = NetworkSelection::Interface("Wi-Fi".into());
        let frame = sampler.sample(&[counter("Ethernet", 1000)], &mode, Duration::from_secs(1));
        assert!(!frame.available);
        assert_eq!(frame.interface.as_deref(), Some("Wi-Fi"));
        assert_eq!(frame.down_bps, 0);
        assert!(NetworkSelection::Interface("".into()).validate().is_err());
    }
}
