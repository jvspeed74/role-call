//! Rollcall Windows platform probe. Not part of any shipped crate.
//!
//! Records, under the conditions of the machine it runs on, the facts behind
//! docs/technical-proposal.md Section 13.2 (A-2, A-3, A-6). Output is ASCII only.
//!
//! Exit codes: 0 ok; 1 an operating-system call failed; 2 usage;
//! 3 anomaly or violated expectation.
//!
//! Provenance model: pointers that the operating system stores inside the buffer
//! passed to GetAdaptersAddresses are treated as carrying the provenance of that
//! buffer, which was exposed to foreign code by passing it. The probe never follows
//! such a pointer unless `Region::holds` shows it lies inside that buffer.

use std::process::ExitCode;

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("windows-platform-probe runs only on Windows");
    ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
    win::main()
}

#[cfg(windows)]
mod win {
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write};
    use std::mem::{align_of, offset_of, size_of};
    use std::net::Ipv4Addr;
    use std::path::Path;
    use std::process::ExitCode;
    use std::ptr;
    use std::sync::Barrier;
    use std::thread;

    use windows_sys::Win32::Foundation::{
        ERROR_ADDRESS_NOT_ASSOCIATED, ERROR_BUFFER_OVERFLOW, ERROR_FILE_EXISTS,
        ERROR_INVALID_PARAMETER, ERROR_NO_DATA, ERROR_NOT_ENOUGH_MEMORY, ERROR_SUCCESS,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        DNS_INTERFACE_SETTINGS, DNS_INTERFACE_SETTINGS_VERSION1, DNS_SETTING_DOMAIN,
        DNS_SETTING_SEARCHLIST, GAA_FLAG_INCLUDE_ALL_INTERFACES, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
        IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_ADDRESSES_LH_0_0, IP_ADAPTER_DNS_SUFFIX,
        IP_ADAPTER_UNICAST_ADDRESS_LH, SetInterfaceDnsSettings,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKET_ADDRESS,
    };
    use windows_sys::core::GUID;

    /// Initial buffer size that Microsoft recommends for GetAdaptersAddresses.
    const WORKING_BUFFER_BYTES: usize = 15_000;
    /// Attempts on ERROR_BUFFER_OVERFLOW; the list can grow between calls.
    const MAX_ATTEMPTS: u32 = 4;
    /// Bound on every linked-list walk, so a corrupt or cyclic list cannot hang the job.
    const MAX_NODES: usize = 4096;

    // The buffer is a Vec<u64>. These assertions tie its start alignment to what the
    // bindings declare for the structures the operating system places in it.
    const _: () = assert!(align_of::<IP_ADAPTER_ADDRESSES_LH>() <= align_of::<u64>());
    const _: () = assert!(align_of::<IP_ADAPTER_UNICAST_ADDRESS_LH>() <= align_of::<u64>());
    const _: () = assert!(align_of::<IP_ADAPTER_DNS_SUFFIX>() <= align_of::<u64>());
    const _: () = assert!(size_of::<IP_ADAPTER_DNS_SUFFIX>() == size_of::<usize>() + 512);

    pub fn main() -> ExitCode {
        let owned: Vec<String> = std::env::args().skip(1).collect();
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        match args.as_slice() {
            ["adapters", mode] => adapters(mode),
            ["set-dns", name, domain, search] => set_dns(name, domain, search),
            ["profile"] => profile(),
            ["create-new", dir, rounds, threads] => create_new(Path::new(dir), rounds, threads),
            _ => {
                eprintln!(
                    "usage: windows-platform-probe adapters <unspec|inet|unspec-all>\n\
                     \x20      windows-platform-probe set-dns <ADAPTER_NAME> <DOMAIN> <SEARCH_LIST>\n\
                     \x20      windows-platform-probe profile\n\
                     \x20      windows-platform-probe create-new <EMPTY_DIR> <ROUNDS> <THREADS>"
                );
                ExitCode::from(2)
            }
        }
    }

    // ---------- rendering ----------

    /// Renders text as an ASCII-only quoted literal (non-ASCII becomes \u{..}).
    fn quote(s: &str) -> String {
        format!("\"{}\"", s.escape_default())
    }

    fn hex16(units: &[u16]) -> String {
        units
            .iter()
            .map(|u| format!("{u:04x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn hex8(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn render_wide(units: &[u16]) -> String {
        format!(
            "{} utf16_valid={} units=[{}]",
            quote(&String::from_utf16_lossy(units)),
            String::from_utf16(units).is_ok(),
            hex16(units)
        )
    }

    fn error_name(code: u32) -> &'static str {
        match code {
            ERROR_SUCCESS => "ERROR_SUCCESS",
            ERROR_BUFFER_OVERFLOW => "ERROR_BUFFER_OVERFLOW",
            ERROR_NO_DATA => "ERROR_NO_DATA",
            ERROR_ADDRESS_NOT_ASSOCIATED => "ERROR_ADDRESS_NOT_ASSOCIATED",
            ERROR_INVALID_PARAMETER => "ERROR_INVALID_PARAMETER",
            ERROR_NOT_ENOUGH_MEMORY => "ERROR_NOT_ENOUGH_MEMORY",
            _ => "other",
        }
    }

    /// Names from the IfType table of the IP_ADAPTER_ADDRESSES_LH documentation, plus 53.
    fn if_type_name(t: u32) -> &'static str {
        match t {
            1 => "IF_TYPE_OTHER",
            6 => "IF_TYPE_ETHERNET_CSMACD",
            9 => "IF_TYPE_ISO88025_TOKENRING",
            23 => "IF_TYPE_PPP",
            24 => "IF_TYPE_SOFTWARE_LOOPBACK",
            37 => "IF_TYPE_ATM",
            53 => "IF_TYPE_PROP_VIRTUAL",
            71 => "IF_TYPE_IEEE80211",
            131 => "IF_TYPE_TUNNEL",
            144 => "IF_TYPE_IEEE1394",
            _ => "unlisted",
        }
    }

    fn oper_status_name(s: i32) -> &'static str {
        match s {
            1 => "IfOperStatusUp",
            2 => "IfOperStatusDown",
            3 => "IfOperStatusTesting",
            4 => "IfOperStatusUnknown",
            5 => "IfOperStatusDormant",
            6 => "IfOperStatusNotPresent",
            7 => "IfOperStatusLowerLayerDown",
            _ => "unlisted",
        }
    }

    // ---------- the buffer and its bounds ----------

    /// The byte range of the buffer that GetAdaptersAddresses filled.
    struct Region {
        start: usize,
        end: usize,
    }

    impl Region {
        /// Whether `len` bytes at `p` lie inside the buffer and `p` is aligned for `T`.
        /// Compares addresses only; never dereferences.
        fn holds<T>(&self, p: *const T, len: usize) -> bool {
            let a = p.addr();
            a >= self.start
                && a.checked_add(len).is_some_and(|e| e <= self.end)
                && a.is_multiple_of(align_of::<T>())
        }
    }

    /// A successful result. `words` owns the memory that every list pointer refers to.
    /// After the call it is never indexed, written, or resized; it is only kept alive
    /// and asked for its length, which does not create a reference to its elements.
    struct Fetched {
        words: Vec<u64>,
        base: *const IP_ADAPTER_ADDRESSES_LH,
        attempts: u32,
    }

    impl Fetched {
        fn region(&self) -> Region {
            let start = self.base.addr();
            Region {
                start,
                end: start + self.words.len() * size_of::<u64>(),
            }
        }
    }

    fn fetch(family: u32, flags: u32) -> Result<Fetched, (u32, u32)> {
        let mut request = WORKING_BUFFER_BYTES;
        for attempt in 1..=MAX_ATTEMPTS {
            let mut words = vec![0u64; request.div_ceil(size_of::<u64>())];
            let capacity = words.len() * size_of::<u64>();
            // Reporting a size smaller than the buffer is safe; reporting a larger one never is.
            let mut size = u32::try_from(capacity).unwrap_or(u32::MAX);
            let base = words.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            // SAFETY: `base` is the start of `words`' heap buffer: non-null, 8-aligned
            // (Vec<u64>; the const assertions show no structure placed there needs more),
            // and `capacity` bytes long. `size` is at most `capacity`, so the function writes
            // only inside the buffer. `words` is neither used nor moved during the call, so
            // the buffer is live and not otherwise accessed. `Reserved` is null, as the
            // documentation requires. The call is synchronous and keeps no pointer afterward.
            let rc = unsafe { GetAdaptersAddresses(family, flags, ptr::null(), base, &mut size) };
            match rc {
                ERROR_SUCCESS => {
                    return Ok(Fetched {
                        words,
                        base: base.cast_const(),
                        attempts: attempt,
                    });
                }
                ERROR_BUFFER_OVERFLOW => {
                    println!(
                        "attempt={attempt} os_error={rc} buffer_bytes={capacity} required_bytes={size}"
                    );
                    // Guard against a non-growing request, which would retry without progress.
                    request = usize::try_from(size)
                        .unwrap_or(usize::MAX)
                        .max(capacity + 1);
                }
                other => return Err((other, attempt)),
            }
        }
        Err((ERROR_BUFFER_OVERFLOW, MAX_ATTEMPTS))
    }

    /// Code units of NUL-terminated OS strings. Implemented only for u8 and u16, for which
    /// every initialized bit pattern is a valid value. The trait is private to this module.
    trait CodeUnit: Copy + Eq {
        const NUL: Self;
    }
    impl CodeUnit for u8 {
        const NUL: Self = 0;
    }
    impl CodeUnit for u16 {
        const NUL: Self = 0;
    }

    /// Reads a NUL-terminated string at `p`, never past the buffer. Ok(None) means null.
    fn read_terminated<T: CodeUnit>(
        region: &Region,
        p: *const T,
    ) -> Result<Option<Vec<T>>, String> {
        if p.is_null() {
            return Ok(None);
        }
        let mut units = Vec::new();
        let mut q = p;
        loop {
            if !region.holds(q, size_of::<T>()) {
                return Err(format!(
                    "outside-buffer-misaligned-or-unterminated addr=0x{:x} units_read={}",
                    q.addr(),
                    units.len()
                ));
            }
            // SAFETY: `holds` showed `q` is aligned for `T` and its size_of::<T>() bytes lie
            // inside the live buffer, which is fully initialized (zeroed, then written by the
            // OS). `T` is u8 or u16 (CodeUnit has no other implementations), for which every
            // bit pattern is valid. Nothing writes the buffer during the read.
            let unit = unsafe { q.read() };
            if unit == T::NUL {
                return Ok(Some(units));
            }
            units.push(unit);
            q = q.wrapping_add(1);
        }
    }

    // ---------- A-2: adapters ----------

    fn adapters(mode: &str) -> ExitCode {
        let base_flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let (family, flags) = match mode {
            "unspec" => (u32::from(AF_UNSPEC), base_flags),
            "inet" => (u32::from(AF_INET), base_flags),
            "unspec-all" => (
                u32::from(AF_UNSPEC),
                base_flags | GAA_FLAG_INCLUDE_ALL_INTERFACES,
            ),
            _ => {
                eprintln!("unknown mode {}", quote(mode));
                return ExitCode::from(2);
            }
        };
        println!(
            "probe=adapters mode={mode} family={family} flags=0x{flags:04x} arch={} \
             size_of_adapter={} offset_of_first_dns_suffix={} size_of_dns_suffix={} size_of_unicast={}",
            std::env::consts::ARCH,
            size_of::<IP_ADAPTER_ADDRESSES_LH>(),
            offset_of!(IP_ADAPTER_ADDRESSES_LH, FirstDnsSuffix),
            size_of::<IP_ADAPTER_DNS_SUFFIX>(),
            size_of::<IP_ADAPTER_UNICAST_ADDRESS_LH>(),
        );
        let fetched = match fetch(family, flags) {
            Ok(f) => f,
            Err((code, attempts)) => {
                println!(
                    "result=inspection-failure step=GetAdaptersAddresses os_error={code} name={} attempts={attempts}",
                    error_name(code)
                );
                return ExitCode::from(1);
            }
        };
        let region = fetched.region();
        println!(
            "result=ok attempts={} buffer_bytes={}",
            fetched.attempts,
            region.end - region.start
        );

        let mut anomalies = 0usize;
        let mut node = fetched.base;
        let mut index = 0usize;
        while !node.is_null() {
            if index == MAX_NODES {
                println!("anomaly=adapter-list-longer-than-{MAX_NODES}");
                anomalies += 1;
                break;
            }
            match read_adapter(&region, node) {
                Ok((adapter, header)) => {
                    print_adapter(index, &adapter, header, &region, &mut anomalies);
                    node = adapter.Next.cast_const();
                }
                Err(why) => {
                    println!("adapter[{index}] anomaly={why}");
                    anomalies += 1;
                    break;
                }
            }
            index += 1;
        }
        println!("adapter_count={index} anomalies={anomalies}");
        // Every pointer derived from the buffer is dead from here on.
        drop(fetched);
        if anomalies == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(3)
        }
    }

    fn read_adapter(
        region: &Region,
        p: *const IP_ADAPTER_ADDRESSES_LH,
    ) -> Result<(IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_ADDRESSES_LH_0_0), String> {
        if !region.holds(p, size_of::<IP_ADAPTER_ADDRESSES_LH>()) {
            return Err(format!(
                "node-outside-buffer-or-misaligned addr=0x{:x}",
                p.addr()
            ));
        }
        // SAFETY: `holds` showed `p` is aligned for IP_ADAPTER_ADDRESSES_LH and all its
        // bytes lie in the live buffer, which is fully initialized (zeroed, then written by
        // the OS). Every field is an integer, an integer array, a union of integers, or a raw
        // pointer (OperStatus, TunnelType, ConnectionType are i32 aliases in windows-sys),
        // so any initialized bytes form a valid value. Nothing writes the buffer meanwhile.
        let adapter = unsafe { p.read() };
        // SAFETY: both variants of this union are plain integers over the same 8 initialized
        // bytes, so reading the {Length, IfIndex} variant yields valid integers.
        let header = unsafe { adapter.Anonymous1.Anonymous };
        let need = offset_of!(IP_ADAPTER_ADDRESSES_LH, FirstDnsSuffix)
            + size_of::<*mut IP_ADAPTER_DNS_SUFFIX>();
        if !usize::try_from(header.Length).is_ok_and(|l| l >= need) {
            return Err(format!(
                "structure-version-too-old Length={} need={need}",
                header.Length
            ));
        }
        Ok((adapter, header))
    }

    fn print_adapter(
        i: usize,
        a: &IP_ADAPTER_ADDRESSES_LH,
        header: IP_ADAPTER_ADDRESSES_LH_0_0,
        region: &Region,
        anomalies: &mut usize,
    ) {
        println!(
            "adapter[{i}] Length={} IfIndex={} Ipv6IfIndex={}",
            header.Length, header.IfIndex, a.Ipv6IfIndex
        );
        match read_terminated(region, a.AdapterName.cast_const()) {
            Ok(Some(bytes)) => println!(
                "adapter[{i}] AdapterName={} bytes=[{}]",
                quote(&String::from_utf8_lossy(&bytes)),
                hex8(&bytes)
            ),
            Ok(None) => println!("adapter[{i}] AdapterName=<null>"),
            Err(why) => {
                *anomalies += 1;
                println!("adapter[{i}] AdapterName=<anomaly {why}>");
            }
        }
        for (label, p) in [
            ("FriendlyName", a.FriendlyName),
            ("Description", a.Description),
            ("DnsSuffix", a.DnsSuffix),
        ] {
            match read_terminated(region, p.cast_const()) {
                Ok(Some(units)) => println!("adapter[{i}] {label}={}", render_wide(&units)),
                Ok(None) => println!("adapter[{i}] {label}=<null>"),
                Err(why) => {
                    *anomalies += 1;
                    println!("adapter[{i}] {label}=<anomaly {why}>");
                }
            }
        }
        println!(
            "adapter[{i}] IfType={} ({})",
            a.IfType,
            if_type_name(a.IfType)
        );
        println!(
            "adapter[{i}] OperStatus={} ({})",
            a.OperStatus,
            oper_status_name(a.OperStatus)
        );
        println!(
            "adapter[{i}] TunnelType={} ConnectionType={}",
            a.TunnelType, a.ConnectionType
        );
        print_suffixes(i, a.FirstDnsSuffix.cast_const(), region, anomalies);
        print_unicast(i, a.FirstUnicastAddress.cast_const(), region, anomalies);
    }

    fn print_suffixes(
        i: usize,
        mut p: *const IP_ADAPTER_DNS_SUFFIX,
        region: &Region,
        anomalies: &mut usize,
    ) {
        let mut k = 0usize;
        while !p.is_null() {
            if k == MAX_NODES || !region.holds(p, size_of::<IP_ADAPTER_DNS_SUFFIX>()) {
                *anomalies += 1;
                println!(
                    "adapter[{i}] FirstDnsSuffix[{k}]=<anomaly node-outside-buffer-misaligned-or-list-too-long addr=0x{:x}>",
                    p.addr()
                );
                return;
            }
            // SAFETY: `holds` showed `p` is aligned for IP_ADAPTER_DNS_SUFFIX and its 520
            // (64-bit) bytes lie in the live, fully initialized buffer. Its fields are a raw
            // pointer and a [u16; 256], valid for any initialized bytes. No concurrent writes.
            let node = unsafe { p.read() };
            match node.String.iter().position(|&u| u == 0) {
                Some(n) => println!(
                    "adapter[{i}] FirstDnsSuffix[{k}]={}",
                    render_wide(&node.String[..n])
                ),
                None => {
                    *anomalies += 1;
                    println!(
                        "adapter[{i}] FirstDnsSuffix[{k}]=<anomaly unterminated-in-256-units> {}",
                        render_wide(&node.String)
                    );
                }
            }
            p = node.Next.cast_const();
            k += 1;
        }
        println!("adapter[{i}] FirstDnsSuffix_count={k}");
    }

    enum Unicast {
        V4(Ipv4Addr),
        Other(u16),
    }

    fn print_unicast(
        i: usize,
        mut p: *const IP_ADAPTER_UNICAST_ADDRESS_LH,
        region: &Region,
        anomalies: &mut usize,
    ) {
        let need = offset_of!(IP_ADAPTER_UNICAST_ADDRESS_LH, Address) + size_of::<SOCKET_ADDRESS>();
        let mut ipv4 = 0usize;
        let mut other_families: Vec<u16> = Vec::new();
        let mut k = 0usize;
        while !p.is_null() {
            if k == MAX_NODES || !region.holds(p, size_of::<IP_ADAPTER_UNICAST_ADDRESS_LH>()) {
                *anomalies += 1;
                println!(
                    "adapter[{i}] unicast[{k}]=<anomaly node-outside-buffer-misaligned-or-list-too-long addr=0x{:x}>",
                    p.addr()
                );
                return;
            }
            // SAFETY: `holds` showed `p` is aligned for IP_ADAPTER_UNICAST_ADDRESS_LH and all
            // its bytes lie in the live, fully initialized buffer. Its fields are integers,
            // integer unions, i32 enum aliases, and raw pointers. No concurrent writes.
            let node = unsafe { p.read() };
            // SAFETY: both variants of this union are plain integers over the same 8
            // initialized bytes.
            let header = unsafe { node.Anonymous.Anonymous };
            if !usize::try_from(header.Length).is_ok_and(|l| l >= need) {
                *anomalies += 1;
                println!(
                    "adapter[{i}] unicast[{k}]=<anomaly Length={} need={need}>",
                    header.Length
                );
                return;
            }
            match read_unicast(region, &node.Address) {
                Ok(Unicast::V4(addr)) => {
                    println!("adapter[{i}] ipv4[{ipv4}]={addr}");
                    ipv4 += 1;
                }
                Ok(Unicast::Other(family)) => other_families.push(family),
                Err(why) => {
                    *anomalies += 1;
                    println!("adapter[{i}] unicast[{k}]=<anomaly {why}>");
                }
            }
            p = node.Next.cast_const();
            k += 1;
        }
        println!("adapter[{i}] ipv4_count={ipv4} other_family_unicast={other_families:?}");
    }

    fn read_unicast(region: &Region, sa: &SOCKET_ADDRESS) -> Result<Unicast, String> {
        let p = sa.lpSockaddr.cast_const();
        if p.is_null() || !region.holds(p, size_of::<SOCKADDR>()) {
            return Err(format!(
                "sockaddr-null-outside-buffer-or-misaligned addr=0x{:x}",
                p.addr()
            ));
        }
        // SAFETY: `holds` showed 16 in-buffer, 2-aligned, initialized bytes at `p`. SOCKADDR
        // is a u16 and an i8 array, valid for any bytes. Reading only the 16-byte SOCKADDR
        // prefix of a longer (IPv6) sockaddr is in bounds by the same check.
        let family = unsafe { p.read() }.sa_family;
        if family != AF_INET {
            return Ok(Unicast::Other(family));
        }
        let sin = p.cast::<SOCKADDR_IN>();
        let long_enough =
            usize::try_from(sa.iSockaddrLength).is_ok_and(|l| l >= size_of::<SOCKADDR_IN>());
        if !long_enough || !region.holds(sin, size_of::<SOCKADDR_IN>()) {
            return Err(format!(
                "sockaddr_in-short-outside-buffer-or-misaligned iSockaddrLength={}",
                sa.iSockaddrLength
            ));
        }
        // SAFETY: the OS reports this sockaddr as AF_INET with at least size_of::<SOCKADDR_IN>()
        // bytes; `holds` showed they are in-buffer, 4-aligned, and initialized. SOCKADDR_IN
        // holds only integers and an integer union.
        let v4 = unsafe { sin.read() };
        // SAFETY: every variant of IN_ADDR's union is integers over the same 4 initialized bytes.
        let s_addr = unsafe { v4.sin_addr.S_un.S_addr };
        // S_addr is in network order, so its in-memory bytes are the dotted octets in order.
        Ok(Unicast::V4(Ipv4Addr::from(s_addr.to_ne_bytes())))
    }

    // ---------- A-2 controlled configuration ----------

    fn parse_guid(text: &str) -> Option<GUID> {
        let inner = text.strip_prefix('{')?.strip_suffix('}')?;
        let groups: Vec<&str> = inner.split('-').collect();
        let lengths: Vec<usize> = groups.iter().map(|g| g.len()).collect();
        if lengths != [8, 4, 4, 4, 12] || !inner.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
        {
            return None;
        }
        u128::from_str_radix(&groups.concat(), 16)
            .ok()
            .map(GUID::from_u128)
    }

    fn wide_z(s: &str) -> Option<Vec<u16>> {
        if s.contains('\0') {
            return None;
        }
        Some(s.encode_utf16().chain(std::iter::once(0)).collect())
    }

    fn set_dns(name: &str, domain: &str, search: &str) -> ExitCode {
        let Some(guid) = parse_guid(name) else {
            println!(
                "probe=set-dns result=bad-adapter-name adapter_name={}",
                quote(name)
            );
            return ExitCode::from(2);
        };
        let (Some(mut d), Some(mut s)) = (wide_z(domain), wide_z(search)) else {
            println!("probe=set-dns result=interior-nul");
            return ExitCode::from(2);
        };
        let settings = DNS_INTERFACE_SETTINGS {
            Version: DNS_INTERFACE_SETTINGS_VERSION1,
            Flags: u64::from(DNS_SETTING_DOMAIN | DNS_SETTING_SEARCHLIST),
            Domain: d.as_mut_ptr(),
            NameServer: ptr::null_mut(),
            SearchList: s.as_mut_ptr(),
            RegistrationEnabled: 0,
            RegisterAdapterName: 0,
            EnableLLMNR: 0,
            QueryAdapterName: 0,
            ProfileNameServer: ptr::null_mut(),
        };
        // SAFETY: `settings` is a live, aligned DNS_INTERFACE_SETTINGS for the whole call.
        // Domain and SearchList point to NUL-terminated UTF-16 buffers owned by `d` and `s`,
        // which outlive the call and contain no interior NUL (checked by wide_z); they were
        // obtained mutably, so even a write by the callee would be permitted. The other
        // pointer fields are null and their flags (NAMESERVER, PROFILE_NAMESERVER) are unset.
        // The function takes the struct by const pointer and keeps no pointer after returning.
        let rc = unsafe { SetInterfaceDnsSettings(guid, &settings) };
        println!(
            "probe=set-dns adapter_name={} domain={} search={} os_error={rc}",
            quote(name),
            quote(domain),
            quote(search)
        );
        if rc == ERROR_SUCCESS {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }

    // ---------- A-3: profile directory ----------

    fn profile() -> ExitCode {
        println!("probe=profile");
        for key in ["USERPROFILE", "HOME", "HOMEDRIVE", "HOMEPATH"] {
            match std::env::var_os(key) {
                Some(v) => println!(
                    "env.{key}={} lossless={}",
                    quote(&v.to_string_lossy()),
                    v.to_str().is_some()
                ),
                None => println!("env.{key}=<unset>"),
            }
        }
        match std::env::home_dir() {
            Some(p) => println!("std_env_home_dir={}", quote(&p.to_string_lossy())),
            None => println!("std_env_home_dir=<none>"),
        }
        ExitCode::SUCCESS
    }

    // ---------- A-6: exclusive creation ----------

    fn open_new(path: &Path) -> io::Result<fs::File> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    fn describe(r: &io::Result<fs::File>) -> String {
        match r {
            Ok(_) => "outcome=created".to_owned(),
            Err(e) => format!(
                "outcome=error kind={:?} os_error={:?}",
                e.kind(),
                e.raw_os_error()
            ),
        }
    }

    fn create_new(dir: &Path, rounds: &str, threads: &str) -> ExitCode {
        let (Ok(rounds), Ok(threads)) = (rounds.parse::<usize>(), threads.parse::<usize>()) else {
            eprintln!("ROUNDS and THREADS must be non-negative integers");
            return ExitCode::from(2);
        };
        if threads == 0 {
            eprintln!("THREADS must be at least 1");
            return ExitCode::from(2);
        }
        println!("probe=create-new dir={}", quote(&dir.to_string_lossy()));
        let mut violations = 0usize;

        // Case 1: a path that does not exist.
        let target = dir.join("a6-existing.bin");
        let first = open_new(&target);
        println!("a6.fresh {}", describe(&first));
        match first {
            Ok(mut f) => {
                if let Err(e) = f.write_all(b"original").and_then(|()| f.sync_all()) {
                    violations += 1;
                    println!(
                        "a6.fresh write_failed kind={:?} os_error={:?}",
                        e.kind(),
                        e.raw_os_error()
                    );
                }
            }
            Err(_) => violations += 1,
        }

        // Case 2: the same path, which now holds a file. Expected: AlreadyExists, code 80.
        let second = open_new(&target);
        println!("a6.existing_file {}", describe(&second));
        let expected = matches!(&second, Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists
                && e.raw_os_error().and_then(|c| u32::try_from(c).ok()) == Some(ERROR_FILE_EXISTS));
        if !expected {
            violations += 1;
        }
        drop(second);
        let unchanged = fs::read(&target).is_ok_and(|b| b == b"original");
        println!("a6.existing_file content_unchanged={unchanged}");
        if !unchanged {
            violations += 1;
        }

        // Case 3: an existing directory (measured; no expectation).
        let d = dir.join("a6-directory");
        match fs::create_dir(&d) {
            Ok(()) => println!("a6.existing_directory {}", describe(&open_new(&d))),
            Err(e) => println!(
                "a6.existing_directory setup_failed kind={:?} os_error={:?}",
                e.kind(),
                e.raw_os_error()
            ),
        }

        // Case 4: an existing read-only file (measured; no expectation).
        let ro = dir.join("a6-readonly.bin");
        let setup = fs::write(&ro, b"original").and_then(|()| {
            let mut perm = fs::metadata(&ro)?.permissions();
            perm.set_readonly(true);
            fs::set_permissions(&ro, perm)
        });
        match setup {
            Ok(()) => println!("a6.existing_readonly_file {}", describe(&open_new(&ro))),
            Err(e) => println!(
                "a6.existing_readonly_file setup_failed kind={:?} os_error={:?}",
                e.kind(),
                e.raw_os_error()
            ),
        }

        // Case 5: a dangling file symbolic link (measured; no expectation).
        let link = dir.join("a6-dangling-link");
        let missing = dir.join("a6-link-target-never-created");
        match std::os::windows::fs::symlink_file(&missing, &link) {
            Ok(()) => {
                println!("a6.dangling_symlink {}", describe(&open_new(&link)));
                println!("a6.dangling_symlink target_created={}", missing.exists());
            }
            Err(e) => println!(
                "a6.dangling_symlink setup_failed kind={:?} os_error={:?}",
                e.kind(),
                e.raw_os_error()
            ),
        }

        // Case 6: concurrent exclusive creation, one fresh path per round.
        let mut histogram = vec![0usize; threads + 1];
        let mut other_errors = 0usize;
        for round in 0..rounds {
            let path = dir.join(format!("a6-race-{round}.bin"));
            let barrier = Barrier::new(threads);
            let (path, barrier) = (&path, &barrier);
            let outcomes: Vec<io::Result<()>> = thread::scope(|s| {
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        s.spawn(move || {
                            barrier.wait();
                            open_new(path).map(drop)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("race thread panicked"))
                    .collect()
            });
            histogram[outcomes.iter().filter(|o| o.is_ok()).count()] += 1;
            for e in outcomes.iter().filter_map(|o| o.as_ref().err()) {
                if e.kind() != io::ErrorKind::AlreadyExists {
                    other_errors += 1;
                    if other_errors <= 10 {
                        println!(
                            "a6.race round={round} unexpected kind={:?} os_error={:?}",
                            e.kind(),
                            e.raw_os_error()
                        );
                    }
                }
            }
        }
        println!(
            "a6.race rounds={rounds} threads={threads} created_per_round_histogram={histogram:?} other_errors={other_errors}"
        );
        if histogram.get(1) != Some(&rounds) || other_errors != 0 {
            violations += 1;
        }
        println!("a6.violations={violations}");
        if violations == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(3)
        }
    }
}
