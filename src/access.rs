//! When and from where a user may use the portal.
//!
//! Stored per user as JSON in `users.access_rules`; missing or empty means any
//! time, from anywhere. Checked when the user signs in, when a session opens,
//! and every minute while one is open (a session is ended when its hours end).
//!
//! Times are this computer's local time. Addresses are the browser's public
//! address as Cloudflare reports it; the country is Cloudflare's guess from it
//! (`CF-IPCountry`). Someone on the host itself (no front end) is never refused
//! for their address or country.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccessRules {
    /// Days allowed, 0 = Sunday ... 6 = Saturday. `None`: every day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<u8>>,
    /// Allowed from this time of day, "HH:MM". Both `from` and `to`, or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Allowed until this time of day, "HH:MM" (may be past midnight: 22:00–06:00).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Allowed addresses or ranges ("203.0.113.7", "203.0.113.0/24"). Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Allowed countries as two-letter codes ("IN"). Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub countries: Vec<String>,
}

/// Where a request came from.
pub struct Origin<'a> {
    /// The browser's address, or "local" when it came from this computer.
    pub address: &'a str,
    /// Two-letter country code from the front end, if it sent one.
    pub country: Option<&'a str>,
}

fn parse_time(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn parse_net(s: &str) -> Option<(IpAddr, u8)> {
    let s = s.trim();
    let (ip, bits) = match s.split_once('/') {
        Some((ip, bits)) => (ip.trim().parse::<IpAddr>().ok()?, Some(bits.trim().parse::<u8>().ok()?)),
        None => (s.parse::<IpAddr>().ok()?, None),
    };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits = bits.unwrap_or(max);
    (bits <= max).then_some((ip, bits))
}

fn in_net(ip: IpAddr, (net, bits): (IpAddr, u8)) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            u128::from(a) & mask == u128::from(b) & mask
        }
        // An IPv4 client seen as IPv4-mapped IPv6, or the other way round.
        (IpAddr::V6(a), IpAddr::V4(_)) => a.to_ipv4_mapped().is_some_and(|a| in_net(IpAddr::V4(a), (net, bits))),
        (IpAddr::V4(a), IpAddr::V6(_)) => in_net(IpAddr::V6(a.to_ipv6_mapped()), (net, bits)),
    }
}

impl AccessRules {
    pub fn parse(json: Option<&str>) -> Self {
        json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default()
    }

    pub fn is_open(&self) -> bool {
        *self == Self::default()
    }

    /// Clean up what the app sent, or say what is wrong with it.
    pub fn normalized(mut self) -> Result<Self, String> {
        if let Some(days) = &mut self.days {
            days.retain(|d| *d < 7);
            days.sort_unstable();
            days.dedup();
            if days.len() == 7 {
                self.days = None;
            } else if days.is_empty() {
                return Err("Pick at least one day.".into());
            }
        }
        self.from = self.from.filter(|s| !s.trim().is_empty());
        self.to = self.to.filter(|s| !s.trim().is_empty());
        match (&self.from, &self.to) {
            (None, None) => {}
            (Some(f), Some(t)) => {
                let (f, t) = (parse_time(f), parse_time(t));
                match (f, t) {
                    (Some(f), Some(t)) if f != t => {
                        self.from = Some(format!("{:02}:{:02}", f / 60, f % 60));
                        self.to = Some(format!("{:02}:{:02}", t / 60, t % 60));
                    }
                    (Some(_), Some(_)) => return Err("The start and end times are the same.".into()),
                    _ => return Err("Times look like 09:00 or 18:30.".into()),
                }
            }
            _ => return Err("Give both a start and an end time, or neither.".into()),
        }
        let mut ips = Vec::new();
        for entry in self.ips.iter().flat_map(|s| s.split([',', '\n', ' '])) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if parse_net(entry).is_none() {
                return Err(format!("“{entry}” isn't an address or range like 203.0.113.7 or 203.0.113.0/24."));
            }
            ips.push(entry.to_string());
        }
        self.ips = ips;
        let mut countries = Vec::new();
        for entry in self.countries.iter().flat_map(|s| s.split([',', '\n', ' '])) {
            let code = entry.trim().to_ascii_uppercase();
            if code.is_empty() {
                continue;
            }
            if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(format!("“{code}” isn't a two-letter country code like IN or US."));
            }
            if !countries.contains(&code) {
                countries.push(code);
            }
        }
        self.countries = countries;
        Ok(self)
    }

    /// Is the user allowed in at this local time? Says when they are, if not.
    pub fn check_time(&self, now: crate::clock::LocalTime) -> Result<(), String> {
        let day_ok = |weekday: u8| self.days.as_ref().is_none_or(|d| d.contains(&weekday));
        let ok = match (self.from.as_deref().and_then(parse_time), self.to.as_deref().and_then(parse_time)) {
            (Some(from), Some(to)) => {
                let m = now.minutes();
                if from < to {
                    day_ok(now.weekday) && m >= from && m < to
                } else if m >= from {
                    // A night shift (22:00–06:00) belongs to the day it starts on.
                    day_ok(now.weekday)
                } else {
                    m < to && day_ok((now.weekday + 6) % 7)
                }
            }
            _ => day_ok(now.weekday),
        };
        if ok {
            Ok(())
        } else {
            Err(format!("You can only use the portal {}.", self.describe_time()))
        }
    }

    /// Is the user allowed in from this place?
    pub fn check_place(&self, origin: &Origin<'_>) -> Result<(), String> {
        if origin.address == "local" {
            return Ok(());
        }
        if !self.ips.is_empty() {
            let allowed = origin
                .address
                .parse::<IpAddr>()
                .ok()
                .is_some_and(|ip| self.ips.iter().filter_map(|n| parse_net(n)).any(|n| in_net(ip, n)));
            if !allowed {
                return Err("You can't use the portal from this network. Ask your administrator.".into());
            }
        }
        if !self.countries.is_empty() {
            let allowed = origin
                .country
                .is_some_and(|c| self.countries.iter().any(|a| a.eq_ignore_ascii_case(c.trim())));
            if !allowed {
                return Err("You can't use the portal from this country. Ask your administrator.".into());
            }
        }
        Ok(())
    }

    pub fn check(&self, origin: &Origin<'_>) -> Result<(), String> {
        self.check_place(origin)?;
        self.check_time(crate::clock::now())
    }

    /// "on Mon–Fri, 09:00–18:00", "every day, 22:00–06:00", "on Sat and Sun".
    pub fn describe_time(&self) -> String {
        let days = match &self.days {
            None => "every day".to_string(),
            Some(d) => {
                // Monday-first runs read best: Mon–Fri, Sat and Sun, Mon, Wed, Fri.
                let order: Vec<u8> = (1..7).chain(std::iter::once(0)).filter(|x| d.contains(x)).collect();
                let pos = |x: u8| (x + 6) % 7;
                let consecutive = order.windows(2).all(|w| pos(w[1]) == pos(w[0]) + 1);
                let names: Vec<&str> = order.iter().map(|x| DAY_NAMES[*x as usize]).collect();
                if consecutive && names.len() > 2 {
                    format!("on {}–{}", names[0], names[names.len() - 1])
                } else if names.len() == 2 {
                    format!("on {} and {}", names[0], names[1])
                } else {
                    format!("on {}", names.join(", "))
                }
            }
        };
        match (&self.from, &self.to) {
            (Some(f), Some(t)) => format!("{days}, {f}–{t}"),
            _ => days,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::LocalTime;

    fn at(weekday: u8, hour: u16, minute: u16) -> LocalTime {
        LocalTime { year: 2026, month: 11, weekday, day: 1, hour, minute, second: 0 }
    }

    #[test]
    fn office_hours() {
        let r = AccessRules { days: Some(vec![1, 2, 3, 4, 5]), from: Some("09:00".into()), to: Some("18:00".into()), ..Default::default() }
            .normalized()
            .unwrap();
        assert!(r.check_time(at(1, 9, 0)).is_ok());
        assert!(r.check_time(at(5, 17, 59)).is_ok());
        assert!(r.check_time(at(5, 18, 0)).is_err());
        assert!(r.check_time(at(6, 12, 0)).is_err());
        assert_eq!(r.describe_time(), "on Mon–Fri, 09:00–18:00");
    }

    #[test]
    fn night_shift_belongs_to_its_start_day() {
        let r = AccessRules { days: Some(vec![5]), from: Some("22:00".into()), to: Some("6:00".into()), ..Default::default() }
            .normalized()
            .unwrap();
        assert!(r.check_time(at(5, 23, 0)).is_ok()); // Friday night
        assert!(r.check_time(at(6, 5, 0)).is_ok()); // early Saturday, still Friday's shift
        assert!(r.check_time(at(6, 23, 0)).is_err());
        assert!(r.check_time(at(5, 5, 0)).is_err()); // early Friday is Thursday's shift
    }

    #[test]
    fn places() {
        let r = AccessRules { ips: vec!["203.0.113.0/24, 2001:db8::/32".into()], countries: vec!["in".into()], ..Default::default() }
            .normalized()
            .unwrap();
        assert_eq!(r.countries, vec!["IN"]);
        let ok = Origin { address: "203.0.113.9", country: Some("IN") };
        assert!(r.check_place(&ok).is_ok());
        assert!(r.check_place(&Origin { address: "2001:db8::5", country: Some("IN") }).is_ok());
        assert!(r.check_place(&Origin { address: "198.51.100.1", country: Some("IN") }).is_err());
        assert!(r.check_place(&Origin { address: "203.0.113.9", country: Some("US") }).is_err());
        assert!(r.check_place(&Origin { address: "203.0.113.9", country: None }).is_err());
        assert!(r.check_place(&Origin { address: "local", country: None }).is_ok());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(AccessRules { ips: vec!["300.1.1.1".into()], ..Default::default() }.normalized().is_err());
        assert!(AccessRules { from: Some("09:00".into()), ..Default::default() }.normalized().is_err());
        assert!(AccessRules { days: Some(vec![]), ..Default::default() }.normalized().is_err());
        assert!(AccessRules { countries: vec!["India".into()], ..Default::default() }.normalized().is_err());
        assert!(AccessRules { days: Some((0..7).collect()), ..Default::default() }.normalized().unwrap().is_open());
    }
}
