//! Source IP designation: picks the folder a client is served from based on its IP.
//!
//! When enabled, the served root holds one folder per IPv4 network, named
//! `a.b.c.d_z` for `a.b.c.d/z` (e.g. `10.32.0.0_16`), plus an optional
//! `default` folder for clients that no network matches. A client only sees the
//! contents of its folder, which appear at the root of its requests, so
//! `<root>/10.32.8.0_24/demo` is fetched as `/demo` by `10.32.8.3`.
//! When several networks contain a client, the most specific (longest prefix) wins.

use std::{
    cmp::Reverse,
    error, fmt, fs, io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

/// Folder name that serves clients matching no network folder.
pub const DEFAULT_FOLDER: &str = "default";

/// An IPv4 network with no host bits set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    network: Ipv4Addr,
    prefix_len: u8,
}

impl Ipv4Net {
    /// Returns `None` if `prefix_len` exceeds 32 or `network` has host bits set.
    pub fn new(network: Ipv4Addr, prefix_len: u8) -> Option<Self> {
        if prefix_len > 32 {
            return None;
        }
        let net = Self {
            network,
            prefix_len,
        };
        (u32::from(network) & !net.mask() == 0).then_some(net)
    }

    fn mask(&self) -> u32 {
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix_len))
            .unwrap_or(0)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network)
    }

    /// Parses a folder name of the form `a.b.c.d_z`.
    ///
    /// Only the canonical spelling is accepted (no leading zeros, signs or host
    /// bits), so every network maps to exactly one folder name.
    pub fn from_folder_name(name: &str) -> Option<Self> {
        let (addr, prefix_len) = name.split_once('_')?;
        let net = Self::new(addr.parse().ok()?, prefix_len.parse().ok()?)?;
        (net.folder_name() == name).then_some(net)
    }

    pub fn folder_name(&self) -> String {
        format!("{}_{}", self.network, self.prefix_len)
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix_len)
    }
}

/// Which clients a [`Folder`] serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clients {
    /// Every client; used when designation is disabled.
    All,
    /// IPv4 clients inside the network.
    Network(Ipv4Net),
    /// Clients no [`Clients::Network`] folder matches.
    Unmatched,
}

impl fmt::Display for Clients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Clients::All => f.write_str("all clients"),
            Clients::Network(net) => net.fmt(f),
            Clients::Unmatched => f.write_str("clients matching no other folder"),
        }
    }
}

/// A directory served to a set of clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    pub clients: Clients,
}

/// Maps clients to the folder they are served from.
///
/// Always holds at least one folder, ordered so the first one that accepts a
/// client is the most specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Router {
    folders: Vec<Folder>,
}

/// Result of [`Router::scan`].
#[derive(Debug)]
pub struct Scan {
    pub router: Router,
    /// Root entries that are not designation folders and are never served.
    pub ignored: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum ScanError {
    Io(io::Error),
    /// The root contains no network or `default` folders.
    NoFolders,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Io(err) => err.fmt(f),
            ScanError::NoFolders => write!(
                f,
                "no `a.b.c.d_z` or `{DEFAULT_FOLDER}` folders found to serve"
            ),
        }
    }
}

impl error::Error for ScanError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            ScanError::Io(err) => Some(err),
            ScanError::NoFolders => None,
        }
    }
}

impl From<io::Error> for ScanError {
    fn from(err: io::Error) -> Self {
        ScanError::Io(err)
    }
}

impl Router {
    /// Serves `root` to every client.
    pub fn single(root: impl Into<PathBuf>) -> Self {
        Self {
            folders: vec![Folder {
                path: root.into(),
                clients: Clients::All,
            }],
        }
    }

    /// Builds a router from the designation folders directly inside `root`.
    pub fn scan(root: &Path) -> Result<Scan, ScanError> {
        let mut folders = Vec::new();
        let mut ignored = Vec::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            // `is_dir` follows symlinks, so linked folders are served too.
            match path.file_name().and_then(|name| name.to_str()) {
                Some(name) if path.is_dir() => match folder_clients(name) {
                    Some(clients) => folders.push(Folder { path, clients }),
                    None => ignored.push(path),
                },
                _ => ignored.push(path),
            }
        }
        if folders.is_empty() {
            return Err(ScanError::NoFolders);
        }
        folders.sort_by_key(|folder| match folder.clients {
            // Offset by one so even a /0 network is checked before the default folder.
            Clients::Network(net) => (Reverse(net.prefix_len + 1), u32::from(net.network)),
            Clients::All | Clients::Unmatched => (Reverse(0), 0),
        });
        ignored.sort();
        Ok(Scan {
            router: Self { folders },
            ignored,
        })
    }

    /// Folders in match order; indices are those returned by [`Router::resolve`].
    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    /// Index of the folder serving `source`, or `None` if no folder serves it.
    ///
    /// IPv4-mapped IPv6 addresses are treated as IPv4; other IPv6 clients and
    /// clients with no known address only match [`Clients::Unmatched`] / [`Clients::All`].
    pub fn resolve(&self, source: Option<IpAddr>) -> Option<usize> {
        let ip = source.map(|ip| ip.to_canonical());
        self.folders
            .iter()
            .position(|folder| match (folder.clients, ip) {
                (Clients::All | Clients::Unmatched, _) => true,
                (Clients::Network(net), Some(IpAddr::V4(ip))) => net.contains(ip),
                (Clients::Network(_), _) => false,
            })
    }
}

fn folder_clients(name: &str) -> Option<Clients> {
    if name == DEFAULT_FOLDER {
        Some(Clients::Unmatched)
    } else {
        Ipv4Net::from_folder_name(name).map(Clients::Network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn net(addr: [u8; 4], prefix_len: u8) -> Ipv4Net {
        Ipv4Net::new(addr.into(), prefix_len).unwrap()
    }

    fn ip(addr: &str) -> Option<IpAddr> {
        Some(addr.parse().unwrap())
    }

    /// Creates `names` as directories under a fresh temp root.
    fn root_with_dirs(names: &[&str]) -> tempfile::TempDir {
        let root = tempdir().unwrap();
        for name in names {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        root
    }

    /// Folder name each client resolves to, or `None` when unserved.
    fn resolved_name(router: &Router, source: Option<IpAddr>) -> Option<String> {
        router.resolve(source).map(|index| {
            let path = &router.folders()[index].path;
            path.file_name().unwrap().to_string_lossy().into_owned()
        })
    }

    #[test]
    fn net_rejects_invalid() {
        assert_eq!(Ipv4Net::new([10, 0, 0, 0].into(), 33), None);
        assert_eq!(Ipv4Net::new([10, 32, 8, 1].into(), 24), None);
        assert_eq!(
            Ipv4Net::new([10, 32, 8, 1].into(), 32),
            Some(net([10, 32, 8, 1], 32))
        );
        assert_eq!(
            Ipv4Net::new([0, 0, 0, 0].into(), 0),
            Some(net([0, 0, 0, 0], 0))
        );
        assert_eq!(Ipv4Net::new([1, 0, 0, 0].into(), 0), None);
    }

    #[test]
    fn net_contains() {
        let ten = net([10, 32, 0, 0], 16);
        assert!(ten.contains([10, 32, 0, 0].into()));
        assert!(ten.contains([10, 32, 255, 255].into()));
        assert!(!ten.contains([10, 33, 0, 0].into()));
        assert!(!ten.contains([10, 31, 255, 255].into()));

        let host = net([127, 0, 0, 1], 32);
        assert!(host.contains([127, 0, 0, 1].into()));
        assert!(!host.contains([127, 0, 0, 2].into()));

        let everything = net([0, 0, 0, 0], 0);
        assert!(everything.contains([255, 255, 255, 255].into()));
        assert!(everything.contains([0, 0, 0, 0].into()));
    }

    #[test]
    fn net_display_and_folder_name() {
        let ten = net([10, 32, 0, 0], 16);
        assert_eq!(ten.to_string(), "10.32.0.0/16");
        assert_eq!(ten.folder_name(), "10.32.0.0_16");
    }

    #[test]
    fn parse_folder_names() {
        assert_eq!(
            Ipv4Net::from_folder_name("10.32.0.0_16"),
            Some(net([10, 32, 0, 0], 16))
        );
        assert_eq!(
            Ipv4Net::from_folder_name("127.0.0.1_32"),
            Some(net([127, 0, 0, 1], 32))
        );
        assert_eq!(
            Ipv4Net::from_folder_name("0.0.0.0_0"),
            Some(net([0, 0, 0, 0], 0))
        );
        for name in [
            "",
            "default",
            "10.32.0.0",
            "10.32.0.0_",
            "_16",
            "10.32.0.0/16",
            "10.32.0.0_16_1",
            "10.32.0.0_33",
            "10.32.0.0_+16",
            "10.32.0.0_016",
            "10.32.0.0_ 16",
            "010.32.0.0_16",
            "10.32.0_16",
            "10.32.1.0_16",
            "fe80::_10",
            "boot",
        ] {
            assert_eq!(Ipv4Net::from_folder_name(name), None, "{name:?}");
        }
    }

    #[test]
    fn single_serves_everyone() {
        let router = Router::single("/pxe");
        assert_eq!(
            router.folders(),
            [Folder {
                path: "/pxe".into(),
                clients: Clients::All
            }]
        );
        for source in [ip("10.0.0.1"), ip("::1"), None] {
            assert_eq!(router.resolve(source), Some(0));
        }
    }

    #[test]
    fn scan_orders_most_specific_first() {
        let root = root_with_dirs(&[
            "default",
            "127.0.0.0_8",
            "127.0.1.0_24",
            "127.0.0.0_16",
            "10.0.0.0_16",
            "0.0.0.0_0",
        ]);
        let scan = Router::scan(root.path()).unwrap();
        assert!(scan.ignored.is_empty());
        let folders: Vec<_> = scan
            .router
            .folders()
            .iter()
            .map(|folder| (folder.path.clone(), folder.clients))
            .collect();
        let expected = [
            ("127.0.1.0_24", Clients::Network(net([127, 0, 1, 0], 24))),
            ("10.0.0.0_16", Clients::Network(net([10, 0, 0, 0], 16))),
            ("127.0.0.0_16", Clients::Network(net([127, 0, 0, 0], 16))),
            ("127.0.0.0_8", Clients::Network(net([127, 0, 0, 0], 8))),
            ("0.0.0.0_0", Clients::Network(net([0, 0, 0, 0], 0))),
            ("default", Clients::Unmatched),
        ]
        .map(|(name, clients)| (root.path().join(name), clients));
        assert_eq!(folders, expected);
    }

    #[test]
    fn scan_ignores_non_designation_entries() {
        let root = root_with_dirs(&["127.0.0.0_8", "boot", "10.0.0.1_8", "Default"]);
        fs::write(root.path().join("10.0.0.0_8"), "a file, not a folder").unwrap();
        fs::write(root.path().join("ipxe.efi"), "").unwrap();

        let scan = Router::scan(root.path()).unwrap();
        assert_eq!(
            scan.router.folders(),
            [Folder {
                path: root.path().join("127.0.0.0_8"),
                clients: Clients::Network(net([127, 0, 0, 0], 8)),
            }]
        );
        let expected = ["10.0.0.0_8", "10.0.0.1_8", "Default", "boot", "ipxe.efi"]
            .map(|name| root.path().join(name));
        assert_eq!(scan.ignored, expected);
    }

    #[cfg(unix)]
    #[test]
    fn scan_follows_symlinked_folders() {
        let root = root_with_dirs(&["shared"]);
        std::os::unix::fs::symlink(root.path().join("shared"), root.path().join("default"))
            .unwrap();
        let scan = Router::scan(root.path()).unwrap();
        assert_eq!(scan.router.folders()[0].path, root.path().join("default"));
        assert_eq!(scan.ignored, [root.path().join("shared")]);
    }

    #[test]
    fn scan_errors() {
        let root = root_with_dirs(&["boot"]);
        fs::write(root.path().join("default"), "").unwrap();
        assert!(matches!(
            Router::scan(root.path()),
            Err(ScanError::NoFolders)
        ));
        assert!(matches!(
            Router::scan(&root.path().join("missing")),
            Err(ScanError::Io(_))
        ));
    }

    #[test]
    fn resolve_longest_prefix_then_default() {
        let root = root_with_dirs(&["127.0.0.0_8", "127.0.1.0_24", "127.0.1.7_32", "default"]);
        let router = Router::scan(root.path()).unwrap().router;
        let cases = [
            (ip("127.0.1.7"), "127.0.1.7_32"),
            (ip("127.0.1.8"), "127.0.1.0_24"),
            (ip("127.0.1.255"), "127.0.1.0_24"),
            (ip("127.0.2.1"), "127.0.0.0_8"),
            (ip("127.255.255.255"), "127.0.0.0_8"),
            (ip("128.0.0.1"), "default"),
            (ip("10.32.8.3"), "default"),
            (ip("::ffff:127.0.1.9"), "127.0.1.0_24"),
            (ip("::1"), "default"),
            (None, "default"),
        ];
        for (source, expected) in cases {
            assert_eq!(
                resolved_name(&router, source).as_deref(),
                Some(expected),
                "{source:?}"
            );
        }
    }

    #[test]
    fn resolve_without_default_leaves_unmatched_unserved() {
        let root = root_with_dirs(&["10.32.8.0_24"]);
        let router = Router::scan(root.path()).unwrap().router;
        assert_eq!(
            resolved_name(&router, ip("10.32.8.3")).as_deref(),
            Some("10.32.8.0_24")
        );
        for source in [ip("10.32.9.3"), ip("127.0.0.1"), ip("::1"), None] {
            assert_eq!(router.resolve(source), None, "{source:?}");
        }
    }

    #[test]
    fn clients_display() {
        assert_eq!(Clients::All.to_string(), "all clients");
        assert_eq!(
            Clients::Network(net([10, 32, 0, 0], 16)).to_string(),
            "10.32.0.0/16"
        );
        assert_eq!(
            Clients::Unmatched.to_string(),
            "clients matching no other folder"
        );
    }

    #[test]
    fn scan_error_display() {
        assert_eq!(
            ScanError::NoFolders.to_string(),
            "no `a.b.c.d_z` or `default` folders found to serve"
        );
    }
}
