use hashbrown::HashMap;
use maxminddb::{Reader, geoip2};
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::str::FromStr;

/// IPv6 ranges excluded from GeoIP lookup, as (network address, prefix length) pairs.
///
/// Only ranges that genuinely cannot be geolocated belong here. 2000::/3 (Global Unicast) used to
/// be listed, which suppressed every routable IPv6 address and reported it as "Private" — a claim
/// that is not merely unhelpful but false, since those addresses are public and the shipped
/// GeoLite2 databases resolve them (issue #182). It was removed; the remaining entries are
/// unspecified, link-local, unique-local and multicast, none of which a GeoIP database can
/// meaningfully answer for.
///
/// FC00::/7 (Unique Local) already covers FD00::/8, but both are listed explicitly since they are
/// commonly documented as separate ranges.
const PRIVATE_IPV6_RANGES: [(u128, u32); 5] = [
    (0x0000_0000_0000_0000_0000_0000_0000_0000, 128), // :: (Unspecified)
    (0xfe80_0000_0000_0000_0000_0000_0000_0000, 10),  // FE80::/10 (Link Local Unicast)
    (0xfc00_0000_0000_0000_0000_0000_0000_0000, 7),   // FC00::/7 (Unique Local)
    (0xfd00_0000_0000_0000_0000_0000_0000_0000, 8),   // FD00::/8 (Unique Local)
    (0xff00_0000_0000_0000_0000_0000_0000_0000, 8),   // FF00::/8 (Multicast)
];

fn ipv6_in_range(ip: &Ipv6Addr, network: u128, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    (u128::from(*ip) & mask) == (network & mask)
}

pub fn is_private_ip(target_ip: &IpAddr) -> bool {
    match target_ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => PRIVATE_IPV6_RANGES
            .iter()
            .any(|&(network, prefix)| ipv6_in_range(ip, network, prefix)),
    }
}

pub struct GeoIPSearch {
    asn: Reader<Vec<u8>>,
    asn_cache: HashMap<IpAddr, String>,
    country: Reader<Vec<u8>>,
    country_cache: HashMap<IpAddr, String>,
    city: Reader<Vec<u8>>,
    city_cache: HashMap<IpAddr, String>,
}

impl GeoIPSearch {
    pub fn new(geo_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let asn = Reader::open_readfile(geo_path.join("GeoLite2-ASN.mmdb"))?;
        let country = Reader::open_readfile(geo_path.join("GeoLite2-Country.mmdb"))?;
        let city = Reader::open_readfile(geo_path.join("GeoLite2-City.mmdb"))?;

        Ok(GeoIPSearch {
            asn,
            asn_cache: HashMap::new(),
            country,
            country_cache: HashMap::new(),
            city,
            city_cache: HashMap::new(),
        })
    }

    pub fn convert(&self, ip: &str) -> Option<IpAddr> {
        let ip = if ip.starts_with("::ffff:") {
            ip.replace("::ffff:", "")
        } else {
            ip.to_string()
        };
        if let Ok(ip) = IpAddr::from_str(&ip) {
            return Some(ip);
        }
        None
    }

    pub fn get_asn(&mut self, ip: IpAddr) -> String {
        if ip.is_loopback() {
            return "Local".to_string();
        }
        if is_private_ip(&ip) {
            return "Private".to_string();
        }
        if let Some(asn) = self.asn_cache.get(&ip) {
            return asn.to_string();
        }
        match self.asn.lookup(ip) {
            Ok(asn) => {
                if let Ok(Some(asn)) = asn.decode::<geoip2::Asn>() {
                    let asn_str = asn.autonomous_system_organization.unwrap_or("-");
                    self.asn_cache.insert(ip, asn_str.to_string());
                    asn_str.to_string()
                } else {
                    self.asn_cache.insert(ip, "-".to_string());
                    "-".to_string()
                }
            }
            _ => "-".to_string(),
        }
    }

    pub fn get_country(&mut self, ip: IpAddr) -> String {
        if ip.is_loopback() || is_private_ip(&ip) {
            return "-".to_string();
        }
        if let Some(country) = self.country_cache.get(&ip) {
            return country.to_string();
        }
        match self.country.lookup(ip) {
            Ok(country) => {
                let mut ret = "-";
                if let Ok(Some(country)) = country.decode::<geoip2::Country>() {
                    let name_tree = country.country.names;
                    ret = name_tree.english.unwrap_or("-")
                }
                let ret = ret.to_string();
                self.country_cache.insert(ip, ret.clone());
                ret
            }
            _ => {
                self.country_cache.insert(ip, "-".to_string());
                "-".to_string()
            }
        }
    }

    pub fn get_city(&mut self, ip: IpAddr) -> String {
        if ip.is_loopback() || is_private_ip(&ip) {
            return "-".to_string();
        }
        if let Some(city) = self.city_cache.get(&ip) {
            return city.to_string();
        }
        match self.city.lookup(ip) {
            Ok(city) => {
                let mut ret = "-";
                if let Ok(Some(city)) = city.decode::<geoip2::City>() {
                    let name_tree = city.city.names;
                    ret = name_tree.english.unwrap_or("-")
                }
                let ret = ret.to_string();
                self.city_cache.insert(ip, ret.clone());
                ret
            }
            _ => {
                self.city_cache.insert(ip, "-".to_string());
                "-".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_private_ranges() {
        for addr in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
        ] {
            assert!(
                is_private_ip(&IpAddr::from_str(addr).unwrap()),
                "{addr} should be private"
            );
        }
        for addr in [
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.255.255",
            "1.1.1.1",
        ] {
            assert!(
                !is_private_ip(&IpAddr::from_str(addr).unwrap()),
                "{addr} should not be private"
            );
        }
    }

    /// #182: a routable IPv6 address must reach the databases, not be short-circuited as
    /// "Private". `is_private_ip` alone would not catch a regression here -- the suppression can
    /// come back either through the range list or through the callers' guards -- so this asserts
    /// the values `GeoIPSearch` actually returns.
    ///
    /// The addresses are ones the checked-in GeoLite2 test fixtures resolve; each database covers
    /// a different set, hence three of them. Verified by mutation: re-adding
    /// `(0x2000_…, 3)` to `PRIVATE_IPV6_RANGES` fails every assertion below.
    #[test]
    fn ipv6_global_unicast_is_geolocated() {
        use std::path::Path;

        let mut geo = GeoIPSearch::new(Path::new("test_files/mmdb"))
            .expect("GeoLite2 test .mmdb files must be present under test_files/mmdb/");

        let jp = geo.convert("2001:218::1").expect("2001:218::1 must parse");
        assert_eq!(geo.get_country(jp), "Japan");

        let ch = geo
            .convert("2001:1700::1")
            .expect("2001:1700::1 must parse");
        assert_eq!(geo.get_asn(ch), "Sunrise Communications AG");

        // The address carried by the repo's own Azure fixture
        // (test_files/json/azure_graph_api_format.json, claims.ipaddr).
        let sonet = geo
            .convert("240d:1a:7fe:a000:e4f5:37e6:6cfd:adbd")
            .expect("the Azure fixture address must parse");
        assert_eq!(geo.get_asn(sonet), "So-net Entertainment Corporation");

        // Non-routable IPv6 keeps its placeholders -- removing 2000::/3 must not open the gate
        // for addresses a GeoIP database genuinely cannot answer for.
        let ula = geo
            .convert("fd12:3456::1")
            .expect("fd12:3456::1 must parse");
        assert_eq!(geo.get_asn(ula), "Private");
        assert_eq!(geo.get_country(ula), "-");
        let loopback = geo.convert("::1").expect("::1 must parse");
        assert_eq!(geo.get_asn(loopback), "Local");
    }

    #[test]
    fn ipv6_private_ranges() {
        for addr in ["::", "fe80::1", "fc00::1", "fd12:3456::1", "ff02::1"] {
            assert!(
                is_private_ip(&IpAddr::from_str(addr).unwrap()),
                "{addr} should be private"
            );
        }
        // Addresses outside every listed range must not match. The global-unicast entries here
        // are the point of #182: 2000::/3 was listed as private, so every routable IPv6 address
        // -- Google Public DNS, a Japanese consumer ISP, an Azure sign-in from anywhere -- was
        // reported as "Private" and never reached the database.
        for addr in [
            "::1",
            "64:ff9b::1",
            "100::1",
            "2001:4860:4860::8888",
            "2001:218::1",
            "240d:1a:7fe:a000:e4f5:37e6:6cfd:adbd",
        ] {
            assert!(
                !is_private_ip(&IpAddr::from_str(addr).unwrap()),
                "{addr} should not be private"
            );
        }
    }
}
