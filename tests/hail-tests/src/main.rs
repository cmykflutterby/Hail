use anyhow::{ensure, Context, Result};
use hail::{
    changes, discovery::parse_lsblk, scan_volume, utc_timestamp, watch::is_block_uevent, Action,
    ChangeSource, Config, ConfigReload, CueSheet, CustomEmulators, CustomPath, Disk, DiskChange,
    DiskConfig, DiskConfigStatus, DiskImage, EmulatorKind, EmulatorSession, EventKind, EventLog,
    Exit, ImageKind, LogClock, LogName, LogindSession, Mode, MountMode, MountPlan, OsEventSource,
    PerPlatform, RescanSchedule, Select, Service, SnowMount, Template, TrackedDisk, EXIT_RESTART,
};
use hail::{
    environment_block, merge_environment, parse_environment_block, quote_windows_arg,
    windows_command_line,
};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, UNIX_EPOCH};

mod calls;

struct TestContext {
    root: PathBuf,
    app: PathBuf,
    default_config: PathBuf,
}

fn disk(root: &Path, name: &str) -> Disk {
    Disk {
        device: format!("dev-{name}"),
        label: name.to_string(),
        mount_point: root.join(name),
    }
}

fn config_with(events: &str) -> String {
    format!(
        "[service]\nlog_file = \"test.log.txt\"\n[polling]\ntype = \"usb-volumes\"\nmode = \"interval\"\ninterval_ms = 250\n[emulator]\ntype = \"snow\"\n{events}"
    )
}

fn expect_invalid(text: &str, needle: &str) -> Result<()> {
    match Config::parse(text) {
        Ok(_) => anyhow::bail!("configuration should be rejected ({needle}):\n{text}"),
        Err(error) => {
            let message = format!("{error:#}");
            ensure!(
                message.contains(needle),
                "rejection message {message:?} lacks {needle:?}"
            );
            Ok(())
        }
    }
}

fn snapshots(context: &TestContext) -> Result<()> {
    let first = disk(&context.root, "one");
    ensure!(changes(&[], &[]).is_empty(), "empty snapshots changed");
    ensure!(
        changes(&[], std::slice::from_ref(&first)) == vec![DiskChange::Inserted(first.clone())],
        "insertion not detected"
    );
    ensure!(
        changes(std::slice::from_ref(&first), &[]) == vec![DiskChange::Removed(first.clone())],
        "removal not detected"
    );
    ensure!(
        changes(std::slice::from_ref(&first), std::slice::from_ref(&first)).is_empty(),
        "unchanged volume emitted a change"
    );
    let moved = Disk {
        mount_point: context.root.join("moved"),
        ..first.clone()
    };
    ensure!(
        changes(std::slice::from_ref(&first), std::slice::from_ref(&moved))
            == vec![DiskChange::Removed(first), DiskChange::Inserted(moved)],
        "remount must remove before inserting"
    );

    let lsblk = r#"{"blockdevices":[{"path":"/dev/sda","tran":"usb","label":null,"mountpoints":[null],"children":[{"path":"/dev/sda1","tran":null,"label":"PiUSB","mountpoints":["/media/pi/USB"]}]},{"path":"/dev/nvme0n1","tran":"nvme","label":null,"mountpoints":["/"]}]}"#;
    let disks = parse_lsblk(lsblk, &[])?;
    ensure!(
        disks.len() == 1 && disks[0].label == "PiUSB" && disks[0].device == "/dev/sda1",
        "USB transport inheritance failed: {disks:?}"
    );
    let probed = parse_lsblk(
        r#"{"blockdevices":[{"path":"/dev/sdb","tran":null,"label":null,"mountpoints":["/media/USB"]}]}"#,
        &[PathBuf::from("/dev/sdb")],
    )?;
    ensure!(
        probed.len() == 1 && probed[0].label == "/dev/sdb",
        "probed device/label fallback failed"
    );
    ensure!(
        parse_lsblk("invalid", &[]).is_err(),
        "invalid lsblk JSON accepted"
    );
    println!("PASS volume snapshots and Linux/Raspberry Pi lsblk parsing");
    Ok(())
}

fn configuration(context: &TestContext) -> Result<()> {
    let config = Config::load(&context.default_config)?;
    ensure!(
        config.log_file == "hail.log.txt",
        "default log file changed"
    );
    ensure!(
        config.polling.mode == Mode::OsEvents { settle_ms: 500 },
        "default detection mode changed"
    );
    for kind in EventKind::ALL {
        ensure!(
            !config.chain(kind).is_empty(),
            "default config lacks a {} chain",
            kind.name()
        );
    }
    ensure!(
        config.chain(EventKind::UsbInserted).last() == Some(&Action::EmulatorAutomount),
        "default usb.inserted chain does not automount"
    );
    let controls = Config::parse(&config_with(
        "[[events.\"usb.inserted\"]]\naction = \"emulator.automount\"\n[[events.\"usb.inserted\"]]\naction = \"hail.restart\"\n[[events.\"polling.failed\"]]\naction = \"hail.quit\"\n",
    ))?;
    ensure!(
        controls.chain(EventKind::UsbInserted) == [Action::EmulatorAutomount, Action::Restart]
            && controls.chain(EventKind::PollingFailed) == [Action::Quit],
        "control actions not parsed"
    );
    expect_invalid(
        &config_with("[[events.\"usb.removed\"]]\naction = \"emulator.automount\"\n"),
        "only valid for usb.inserted",
    )?;
    expect_invalid(
        &config_with("[[events.\"usb.removed\"]]\naction = \"hail.quit\"\ncode = 3\n"),
        "code",
    )?;
    expect_invalid(
        &config_with(
            "[[events.\"usb.removed\"]]\naction = \"log.write\"\nmessage = \"{hail_cfg}\"\n",
        ),
        "unknown placeholder {hail_cfg}",
    )?;
    let minimal = Config::parse(&config_with(""))?;
    ensure!(
        minimal.chains.is_empty(),
        "missing events table should be empty"
    );

    let log_write = "[[events.\"usb.inserted\"]]\naction = \"log.write\"\nmessage = \"x\"\n";
    expect_invalid(
        &config_with("[[events.\"usb.inserted\"]]\naction = \"shell.run\"\n"),
        "shell.run",
    )?;
    expect_invalid(
        &config_with("[[events.\"disk.exploded\"]]\naction = \"log.write\"\nmessage = \"x\"\n"),
        "unknown event",
    )?;
    expect_invalid(
        &config_with(
            "[[events.\"usb.inserted\"]]\naction = \"log.write\"\nmessage = \"{error}\"\n",
        ),
        "unknown placeholder {error}",
    )?;
    expect_invalid(
        &config_with(
            "[[events.\"usb.inserted\"]]\naction = \"log.write\"\nmessage = \"x\"\nextra = 1\n",
        ),
        "extra",
    )?;
    expect_invalid(
        &config_with("[[events.\"usb.inserted\"]]\naction = \"log.write\"\nmessage = \"oops }\"\n"),
        "unmatched",
    )?;
    expect_invalid(
        &config_with(log_write).replace("usb-volumes", "carrier-pigeon"),
        "carrier-pigeon",
    )?;
    expect_invalid(&config_with(log_write).replace("250", "0"), "interval_ms")?;
    expect_invalid(
        &config_with(log_write).replace("test.log.txt", "E:\\\\hail.log.txt"),
        "plain file name",
    )?;
    expect_invalid(
        &config_with(log_write).replace("test.log.txt", "../hail.log.txt"),
        "plain file name",
    )?;
    expect_invalid(
        &config_with(log_write).replace("[service]", "[servise]"),
        "servise",
    )?;
    let service =
        |lines: &str| config_with(log_write).replace("log_file = \"test.log.txt\"\n", lines);
    let timed = Config::parse(&service(
        "log_file = \"hail-{date}.log\"\nlog_time = \"utc\"\n",
    ))?;
    ensure!(
        timed.log.as_ref().is_some_and(|name| name.is_timed()
            && name.clock() == LogClock::Utc
            && name.pattern() == "hail-{date}.log"),
        "timed log_file not parsed"
    );
    ensure!(
        Config::parse(&config_with(log_write))?
            .log
            .is_some_and(|name| !name.is_timed() && name.clock() == LogClock::Local),
        "plain log_file defaults wrong"
    );
    ensure!(
        Config::parse(&service("logging = false\n"))?.log.is_none()
            && Config::parse(&service("logging = false\nlog_file = \"x-{day}.log\"\n"))?
                .log
                .is_none()
            && Config::parse(&service("logging = true\nlog_file = \"x.log\"\n"))?
                .log
                .is_some(),
        "logging switch not applied"
    );
    expect_invalid(&service(""), "log_file is required")?;
    expect_invalid(
        &service("logging = \"no\"\nlog_file = \"x.log\"\n"),
        "logging",
    )?;
    expect_invalid(
        &service("logging = false\nlog_file = \"a/{date}.log\"\n"),
        "plain file name",
    )?;
    expect_invalid(
        &service("log_file = \"x-{second}.log\"\n"),
        "unknown placeholder {second}",
    )?;
    expect_invalid(&service("log_file = \"x-{date.log\"\n"), "placeholder")?;
    expect_invalid(
        &service("log_file = \"x.log\"\nlog_time = \"mars\"\n"),
        "mars",
    )?;

    let interval = config_with(log_write);
    let events_mode = |extra: &str| {
        interval.replace(
            "mode = \"interval\"\ninterval_ms = 250\n",
            &format!("mode = \"os-events\"\n{extra}"),
        )
    };
    ensure!(
        Config::parse(&events_mode(""))?.polling.mode == Mode::OsEvents { settle_ms: 500 },
        "os-events default settle_ms wrong"
    );
    ensure!(
        Config::parse(&events_mode("settle_ms = 2000\n"))?
            .polling
            .mode
            == Mode::OsEvents { settle_ms: 2000 },
        "os-events settle_ms not honoured"
    );
    ensure!(
        Config::parse(&interval)?.polling.mode == Mode::Interval { interval_ms: 250 },
        "interval mode not parsed"
    );
    expect_invalid(&events_mode("settle_ms = 10\n"), "settle_ms must be")?;
    expect_invalid(
        &events_mode("interval_ms = 250\n"),
        "applies only to mode = \"interval\"",
    )?;
    expect_invalid(
        &interval.replace(
            "interval_ms = 250\n",
            "interval_ms = 250\nsettle_ms = 500\n",
        ),
        "applies only to mode = \"os-events\"",
    )?;
    expect_invalid(
        &interval.replace("interval_ms = 250\n", ""),
        "requires polling.interval_ms",
    )?;
    expect_invalid(&interval.replace("mode = \"interval\"\n", ""), "mode")?;
    expect_invalid(&interval.replace("\"interval\"", "\"psychic\""), "psychic")?;

    let fields = BTreeMap::from([("label", "A{B}".to_string())]);
    let template = Template::parse("{{literal}} {label}!", &["label"])?;
    ensure!(
        template.render(&fields) == "{literal} A{B}!",
        "template rendering failed"
    );
    ensure!(
        utc_timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_723))
            == "2000-02-29T01:02:03Z",
        "timestamp formatting failed"
    );
    println!("PASS configuration parsing, validation, templates and timestamps");
    Ok(())
}

fn action_chains(context: &TestContext) -> Result<()> {
    let directory = context.root.join("chains");
    fs::create_dir(&directory)?;
    let config = Config::parse(&config_with(
        r#"
[[events."service.started"]]
action = "log.write"
message = "{event} {version}"
[[events."usb.inserted"]]
action = "log.write"
message = "first {label} {mount_point}"
[[events."usb.inserted"]]
action = "log.write"
message = "second {device}"
[[events."polling.failed"]]
action = "log.write"
message = "{event}: {error}"
"#,
    ))?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    service.start(Path::new("cfg.toml"))?;
    let volume = disk(&context.root, "stick");
    let inserted = service.observe(Ok(vec![volume.clone()]))?;
    ensure!(
        inserted.len() == 1 && inserted[0].kind == EventKind::UsbInserted,
        "insert event not raised"
    );
    ensure!(
        service.observe(Ok(vec![volume.clone()]))?.is_empty(),
        "unchanged poll raised events"
    );
    let failed = service.observe(Err(anyhow::anyhow!("bus offline")))?;
    ensure!(
        failed.len() == 1 && failed[0].kind == EventKind::PollingFailed,
        "polling failure not raised"
    );
    let removed = service.observe(Ok(Vec::new()))?;
    ensure!(
        removed.len() == 1 && removed[0].kind == EventKind::UsbRemoved,
        "removal event not raised"
    );
    let path = service.log.path.clone();
    drop(service);
    let expected = format!(
        "service.started {}\nfirst stick {}\nsecond dev-stick\npolling.failed: bus offline\n",
        hail::VERSION,
        volume.mount_point.display()
    );
    let contents = fs::read_to_string(&path)?;
    ensure!(
        contents == expected,
        "log.write chain output wrong:\n{contents}"
    );
    println!("PASS ordered action chains, unconfigured events ignored, polling failures logged");
    Ok(())
}

fn log_location(context: &TestContext) -> Result<()> {
    let binary = context.root.join("binary");
    let temporary = context.root.join("temporary");
    fs::create_dir(&binary)?;
    fs::create_dir(&temporary)?;
    let mut log = EventLog::open(&binary, &temporary, "a.log", &[])?;
    ensure!(log.path == binary.join("a.log"), "log is not beside binary");
    log.write_line("one")?;
    drop(log);
    EventLog::open(&binary, &temporary, "a.log", &[])?.write_line("two")?;
    ensure!(
        fs::read_to_string(binary.join("a.log"))? == "one\ntwo\n",
        "log does not append"
    );

    let blocked = context.root.join("not-a-directory");
    fs::write(&blocked, "blocks the primary log location")?;
    let fallback = EventLog::open(&blocked, &temporary, "b.log", &[])?;
    ensure!(
        fallback.path == temporary.join("b.log"),
        "unwritable fallback location wrong"
    );
    ensure!(
        EventLog::open(&blocked, &context.root.join("absent"), "b.log", &[]).is_err(),
        "unwritable primary and fallback did not fail"
    );

    let usb_root = context.root.join("usb");
    let on_usb = usb_root.join("hail");
    fs::create_dir_all(&on_usb)?;
    let avoided = EventLog::open(
        &on_usb,
        &temporary,
        "c.log",
        std::slice::from_ref(&usb_root),
    )?;
    ensure!(
        avoided.path == temporary.join("c.log") && !on_usb.join("c.log").exists(),
        "log was placed on a detected USB volume"
    );
    ensure!(
        EventLog::open(&on_usb, &usb_root, "d.log", std::slice::from_ref(&usb_root)).is_err()
            && fs::read_dir(&usb_root)?.count() == 1,
        "fallback onto a USB volume was not refused"
    );
    let at = |seconds: u64| UNIX_EPOCH + Duration::from_secs(seconds);
    let all_tokens = LogName::parse(
        "h-{year}{month}{day}-{hour}{minute}-{date}-{week_year}W{week}-{weekday}.log",
        LogClock::Utc,
    )?;
    for (seconds, expected) in [
        // 2026-12-31T23:59Z Thursday: ISO week 53 of 2026.
        (1_798_761_540, "h-20261231-2359-2026-12-31-2026W53-thu.log"),
        // 2024-12-30 Monday belongs to ISO week 1 of 2025.
        (1_735_516_800, "h-20241230-0000-2024-12-30-2025W01-mon.log"),
        (1_799_020_800, "h-20270104-0000-2027-01-04-2027W01-mon.log"),
        (0, "h-19700101-0000-1970-01-01-1970W01-thu.log"),
    ] {
        let rendered = all_tokens.render(at(seconds));
        ensure!(rendered == expected, "log name at {seconds}: {rendered}");
    }
    ensure!(
        LogName::parse("{{x}}-{year}.log", LogClock::Utc)?.render(at(0)) == "{x}-1970.log"
            && !LogName::parse("plain.log", LogClock::Utc)?.is_timed()
            && LogName::parse("{weekday}.log", LogClock::Utc)?.is_timed(),
        "log name escapes / timed detection wrong"
    );
    let now = std::time::SystemTime::now();
    let offset = |civil: hail::CivilTime| {
        hail::days_from_civil(civil.year, civil.month, civil.day) * 86_400
            + i64::from(civil.hour * 3_600 + civil.minute * 60)
    };
    let local_offset = offset(LogClock::Local.civil(now)) - offset(LogClock::Utc.civil(now));
    ensure!(
        local_offset.abs() <= 14 * 3_600 && local_offset % 900 == 0,
        "local time offset {local_offset} s implausible"
    );

    let rolling = context.root.join("rolling");
    fs::create_dir(&rolling)?;
    let t0 = 1_794_108_600; // 2026-11-08T03:30:00Z
    let name = LogName::parse("r-{date}-{hour}.log", LogClock::Utc)?;
    let mut log = EventLog::open_at(&rolling, &temporary, name.clone(), &[], at(t0))?;
    log.write_line_at("a", at(t0))?;
    log.write_line_at("b", at(t0 + 29 * 60))?;
    log.write_line_at("c", at(t0 + 31 * 60))?;
    log.write_line_at("d", at(t0 + 21 * 3_600))?;
    ensure!(
        log.path == rolling.join("r-2026-11-09-00.log"),
        "rolled log path wrong: {}",
        log.path.display()
    );
    drop(log);
    let read = |file: &str| fs::read_to_string(rolling.join(file)).unwrap_or_default();
    ensure!(
        read("r-2026-11-08-03.log") == "a\nb\n"
            && read("r-2026-11-08-04.log") == "c\n"
            && read("r-2026-11-09-00.log") == "d\n"
            && fs::read_dir(&rolling)?.count() == 3,
        "log did not roll over at hour/day boundaries"
    );
    let mut log = EventLog::open_at(&rolling, &temporary, name.clone(), &[], at(t0))?;
    log.write_line_at("e", at(t0 + 60))?;
    drop(log);
    ensure!(
        read("r-2026-11-08-03.log") == "a\nb\ne\n",
        "reopened period log does not append"
    );
    let mut log = EventLog::open_at(&blocked, &temporary, name, &[], at(t0))?;
    log.write_line_at("f", at(t0 + 3_600))?;
    ensure!(
        log.path == temporary.join("r-2026-11-08-04.log")
            && fs::read_to_string(&log.path)? == "f\n",
        "rolled log ignored the temporary fallback"
    );

    let mut disabled = EventLog::disabled();
    disabled.write_line("dropped")?;
    disabled.debug("dropped")?;
    ensure!(
        !disabled.is_enabled() && disabled.path.as_os_str().is_empty(),
        "disabled log not inert"
    );
    println!("PASS host-only log: beside binary, append, temp fallback, USB volumes never written, disabled, time-token names roll over");
    Ok(())
}

fn sized(path: &Path, size: u64) -> Result<()> {
    fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?
        .set_len(size)?;
    Ok(())
}

fn tracked(volume: &Path) -> TrackedDisk {
    TrackedDisk {
        disk: Disk {
            device: "dev-volume".to_string(),
            label: "volume".to_string(),
            mount_point: volume.to_path_buf(),
        },
        inserted_at: "2000-01-01T00:00:00Z".to_string(),
        scan: scan_volume(volume),
    }
}

fn listing(root: &Path) -> Result<Vec<(PathBuf, u64, std::time::SystemTime)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            // NTFS folder timestamps in directory listings refresh lazily; compare contents instead.
            entries.push((path.clone(), 0, UNIX_EPOCH));
            entries.extend(listing(&path)?);
        } else {
            entries.push((path, metadata.len(), metadata.modified()?));
        }
    }
    entries.sort();
    Ok(entries)
}

fn disk_media(context: &TestContext) -> Result<()> {
    let classify = |name: &str, size: u64| ImageKind::classify(Path::new(name), size);
    ensure!(
        classify("a.ISO", 1) == Some(ImageKind::CdRom)
            && classify("a.toast", 1) == Some(ImageKind::CdRom)
            && classify("a.cue", 1) == Some(ImageKind::CdRom),
        "CD-ROM images not classified"
    );
    ensure!(
        classify("a.img", 819_200) == Some(ImageKind::Floppy)
            && classify("a.img", 40 << 20) == Some(ImageKind::HardDisk)
            && classify("a.dsk", 3 << 20) == Some(ImageKind::HardDisk)
            && classify("a.hda", 1) == Some(ImageKind::HardDisk)
            && classify("a.moof", 1) == Some(ImageKind::Floppy)
            && classify("a.bin", 1).is_none()
            && classify("a.txt", 1).is_none()
            && classify("img", 1).is_none(),
        "floppy/hard disk classification wrong"
    );

    let cue = context.root.join("cue");
    fs::create_dir(&cue)?;
    sized(&cue.join("Track 01.bin"), 2352 * 10)?;
    sized(&cue.join("track02.wav"), 44)?;
    let sheet = CueSheet::parse(
        &cue,
        "REM test\nFILE \"Track 01.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nfile track02.wav wave\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n",
    )?;
    ensure!(
        sheet.files.len() == 2
            && sheet.files[0].path == cue.join("Track 01.bin")
            && sheet.files[0].size == 23_520
            && sheet.files[1].format == "WAVE"
            && sheet
                .tracks
                .iter()
                .map(|track| (track.number, track.mode.as_str()))
                .eq([(1, "MODE1/2352"), (2, "AUDIO")]),
        "cue sheet parsed wrongly: {sheet:?}"
    );
    for (text, needle) in [
        (
            "FILE missing.bin BINARY\nTRACK 01 MODE1/2048\n",
            "missing.bin",
        ),
        ("FILE ../x.bin BINARY\nTRACK 01 MODE1/2048\n", "\"..\""),
        (
            "FILE track02.wav MP3\nTRACK 01 AUDIO\n",
            "unsupported FILE type",
        ),
        (
            "FILE track02.wav WAVE\nTRACK 01 MODE9/1\n",
            "unsupported TRACK mode",
        ),
        ("TRACK 01 AUDIO\n", "TRACK before any FILE"),
        ("FILE track02.wav WAVE\n", "no tracks"),
    ] {
        let error = format!(
            "{:#}",
            CueSheet::parse(&cue, text)
                .err()
                .with_context(|| format!("cue sheet accepted: {text:?}"))?
        );
        ensure!(
            error.contains(needle),
            "cue error {error:?} lacks {needle:?}"
        );
    }

    let cfg = |text: &str| DiskConfig::parse(PathBuf::from("hail.cfg"), text);
    let defaults = cfg("")?;
    ensure!(
        !defaults.automount && defaults.select == Select::First && defaults.image.is_none(),
        "hail.cfg defaults wrong"
    );
    ensure!(
        cfg("automount = true\nselect = \"all\"\n")?.select == Select::All
            && cfg("select = \"none\"\n")?.select == Select::None,
        "hail.cfg select not parsed"
    );
    ensure!(
        cfg("automount = true\nimage = '/Disks\\System.hda'\n")?.image
            == Some(Path::new("Disks").join("System.hda")),
        "hail.cfg image path not normalised"
    );
    for select in Select::ALL {
        ensure!(
            cfg(&format!("select = \"{}\"\n", select.name()))?.select == select,
            "hail.cfg select {} not parsed",
            select.name()
        );
    }
    ensure!(
        defaults.mode == MountMode::default(),
        "hail.cfg default mode wrong"
    );
    for (text, readonly, writeback, canonical) in [
        ("readonly", true, None, "readonly"),
        (
            " nowriteback , readonly ",
            true,
            Some(false),
            "readonly,nowriteback",
        ),
        ("writeback", false, Some(true), "writeback"),
        ("", false, None, "default"),
    ] {
        let mode = cfg(&format!("mode = \"{text}\"\n"))?.mode;
        ensure!(
            mode == MountMode {
                readonly,
                writeback
            } && mode.describe() == canonical,
            "hail.cfg mode {text:?} parsed as {mode:?}"
        );
    }
    for (text, needle) in [
        (
            "image = \"a.img\"\nselect = \"all\"\n",
            "either image or select",
        ),
        ("image = \"../a.img\"\n", "\"..\""),
        ("image = \"C:/a.img\"\n", "relative to the disk root"),
        ("image = \"/\"\n", "must name a file"),
        ("select = \"some\"\n", "some"),
        ("select = \"newest\"\n", "last-modified"),
        ("mode = \"readonly,turbo\"\n", "unknown mode flag \"turbo\""),
        ("mode = \"readonly,,nowriteback\"\n", "empty flag"),
        ("mode = \"readonly,readonly\"\n", "repeated"),
        (
            "mode = \"writeback,nowriteback\"\n",
            "writeback and nowriteback",
        ),
        ("mode = \"readonly,writeback\"\n", "readonly and writeback"),
        ("mode = [\"readonly\"]\n", "mode"),
        ("automount = \"yes\"\n", "automount"),
        ("mount = true\n", "mount"),
    ] {
        let error = format!(
            "{:#}",
            cfg(text)
                .err()
                .with_context(|| format!("hail.cfg accepted: {text:?}"))?
        );
        ensure!(
            error.contains(needle),
            "hail.cfg error {error:?} lacks {needle:?}"
        );
    }

    let volume = context.root.join("volume");
    fs::create_dir_all(volume.join("Disks"))?;
    sized(&volume.join("Zeta.iso"), 4096)?;
    sized(&volume.join("alpha.img"), 819_200)?;
    sized(&volume.join("._alpha.img"), 4096)?;
    sized(&volume.join("Big.dsk"), 3 << 20)?;
    sized(&volume.join("Game.bin"), 2352)?;
    fs::write(
        volume.join("Game.cue"),
        "FILE \"Game.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n",
    )?;
    fs::write(
        volume.join("Broken.cue"),
        "FILE \"Lost.bin\" BINARY\n  TRACK 01 MODE1/2352\n",
    )?;
    fs::write(volume.join("notes.txt"), "not an image")?;
    sized(&volume.join("Disks").join("System.hda"), 5 << 20)?;
    let config_file = volume.join("HAIL.CFG");
    fs::write(&config_file, "automount = true\n")?;
    for (name, year) in [
        ("alpha.img", 2001),
        ("Big.dsk", 2002),
        ("Zeta.iso", 2003),
        ("Game.cue", 2004),
    ] {
        let at = |year: u64| UNIX_EPOCH + Duration::from_secs((year - 1970) * 31_557_600);
        let times = fs::FileTimes::new().set_modified(at(year));
        // Creation order is the reverse of modification order (Windows only; Linux cannot set it).
        #[cfg(windows)]
        let times = std::os::windows::fs::FileTimesExt::set_created(times, at(4005 - year));
        fs::File::options()
            .write(true)
            .open(volume.join(name))?
            .set_times(times)?;
    }

    let before = listing(&volume)?;
    let scan = scan_volume(&volume);
    let after = listing(&volume)?;
    ensure!(
        after == before,
        "scanning modified the volume:\nbefore {before:#?}\nafter {after:#?}"
    );
    let names: Vec<_> = scan
        .images
        .iter()
        .map(|image| {
            (
                image
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                image.kind,
            )
        })
        .collect();
    ensure!(
        names
            == [
                ("alpha.img".to_string(), ImageKind::Floppy),
                ("Big.dsk".to_string(), ImageKind::HardDisk),
                ("Game.cue".to_string(), ImageKind::CdRom),
                ("Zeta.iso".to_string(), ImageKind::CdRom),
            ],
        "volume images wrong: {names:?}"
    );
    ensure!(
        scan.images[2]
            .cue
            .as_ref()
            .is_some_and(|sheet| sheet.files[0].path == volume.join("Game.bin"))
            && scan.images[3].cue.is_none(),
        "cue sheet not attached to CD-ROM image"
    );
    ensure!(
        scan.rejected.len() == 1
            && scan.rejected[0].0 == volume.join("Broken.cue")
            && scan.rejected[0].1.contains("Lost.bin"),
        "broken cue sheet not rejected: {:?}",
        scan.rejected
    );
    ensure!(
        matches!(&scan.config, DiskConfigStatus::Found(found) if found.automount && found.path == config_file),
        "hail.cfg not found case-insensitively: {:?}",
        scan.config
    );
    ensure!(
        scan_volume(&context.root.join("absent-volume"))
            .error
            .is_some(),
        "unlistable volume not reported"
    );

    let plan = |text: Option<&str>| -> Result<MountPlan> {
        match text {
            Some(text) => fs::write(&config_file, text)?,
            None => fs::remove_file(&config_file)?,
        }
        Ok(tracked(&volume).mount_plan())
    };
    let mounted = |plan: MountPlan| -> Vec<String> {
        match plan {
            MountPlan::Mount(images) => images
                .iter()
                .map(|image| {
                    image
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect(),
            MountPlan::External {
                emulator, image, ..
            } => {
                vec![format!("{emulator}: {}", image.display())]
            }
            MountPlan::NoHostEmulator { emulator, reason } => {
                vec![format!("no host emulator {emulator}: {reason}")]
            }
            MountPlan::Skip(reason) => vec![format!("skip: {reason}")],
        }
    };
    let cases = [
        (None, vec!["skip: no hail.cfg on disk"]),
        (
            Some("select = \"all\"\n"),
            vec!["skip: hail.cfg automount = false"],
        ),
        (
            Some("automount = true\nselect = \"none\"\n"),
            vec!["skip: hail.cfg select = \"none\""],
        ),
        (Some("automount = true\n"), vec!["alpha.img"]),
        (
            Some("automount = true\nselect = \"all\"\n"),
            vec!["alpha.img", "Big.dsk", "Game.cue", "Zeta.iso"],
        ),
        (
            Some("automount = true\nimage = \"Disks/System.hda\"\n"),
            vec!["System.hda"],
        ),
        (
            Some("automount = true\nselect = \"last-modified\"\n"),
            vec!["Game.cue"],
        ),
        (
            Some("automount = true\nselect = \"largest\"\n"),
            vec!["Big.dsk"],
        ),
        (
            Some("automount = true\nselect = \"smallest\"\n"),
            vec!["Game.cue"],
        ),
        (
            Some("automount = true\nselect = \"oldest-created\"\nmode = \"readonly\"\n"),
            vec![if cfg!(windows) { "Game.cue" } else { "" }],
        ),
    ];
    for (text, expected) in cases {
        let actual = mounted(plan(text)?);
        // Linux filesystems may not record creation times; checked deterministically below.
        if expected == [""] {
            ensure!(actual.len() == 1, "mount plan for {text:?}: {actual:?}");
            continue;
        }
        ensure!(actual == expected, "mount plan for {text:?}: {actual:?}");
    }
    ensure!(
        tracked(&volume).mount_mode()
            == MountMode {
                readonly: true,
                writeback: None
            },
        "tracked disk mount mode not taken from hail.cfg"
    );

    let image = |name: &str, size: u64, modified: Option<u64>, created: Option<u64>| DiskImage {
        path: PathBuf::from(name),
        kind: ImageKind::Floppy,
        size,
        modified: modified.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        created: created.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        cue: None,
    };
    let mut cue_image = image("d.cue", 10, Some(5), None);
    cue_image.kind = ImageKind::CdRom;
    cue_image.cue = Some(sheet.clone());
    let images = [
        image("a", 500, Some(30), Some(10)),
        image("b", 500, None, Some(5)),
        image("c", 100, Some(30), None),
        cue_image,
    ];
    let pick = |select: Select| -> Vec<String> {
        select
            .pick(&images)
            .iter()
            .map(|image| image.path.display().to_string())
            .collect()
    };
    for (select, expected) in [
        (Select::None, vec![]),
        (Select::First, vec!["a"]),
        (Select::All, vec!["a", "b", "c", "d.cue"]),
        // a and c tie on modified: first by name wins; b has no time and is chosen last.
        (Select::LastModified, vec!["a"]),
        // d.cue falls back to its modified time (5) and ties with b's creation (5).
        (Select::OldestCreated, vec!["b"]),
        // d.cue's data size includes its track files (10 + 23564 bytes).
        (Select::Largest, vec!["d.cue"]),
        (Select::Smallest, vec!["c"]),
    ] {
        ensure!(
            pick(select) == expected,
            "select {} picked {:?}",
            select.name(),
            pick(select)
        );
    }
    ensure!(
        images[3].data_size() == 10 + 23_520 + 44
            && Select::LastModified.pick(&images[1..2]).len() == 1
            && Select::Largest.pick(&[]).is_empty(),
        "image selection edge cases wrong"
    );

    let plan_for = |kind: ImageKind, flags: &str| -> Result<SnowMount> {
        let mut image = image("x", 1, None, None);
        image.kind = kind;
        Ok(SnowMount::plan(&image, MountMode::parse(flags)?, None))
    };
    for (kind, flags, commands, note) in [
        (
            ImageKind::Floppy,
            "",
            vec!["InsertFloppy(drive=next free, \"x\", write_protect=false)"],
            "Snow default",
        ),
        (
            ImageKind::Floppy,
            "readonly",
            vec!["InsertFloppy(drive=next free, \"x\", write_protect=true)"],
            "write-protected floppy",
        ),
        (
            ImageKind::Floppy,
            "writeback",
            vec![
                "InsertFloppy(drive=next free, \"x\", write_protect=false)",
                "SetFloppyWriteback(drive, true)",
            ],
            "saved to the image file",
        ),
        (
            ImageKind::HardDisk,
            "",
            vec!["ScsiAttachHdd(id=next free, \"x\")"],
            "in place",
        ),
        (
            ImageKind::HardDisk,
            "nowriteback",
            vec![
                "ScsiAttachHdd(id=next free, \"x\")",
                "ScsiBranchHdd(id, <host temporary copy>)",
            ],
            "not modified",
        ),
        (
            ImageKind::HardDisk,
            "readonly",
            vec![
                "ScsiAttachHdd(id=next free, \"x\")",
                "ScsiBranchHdd(id, <host temporary copy>)",
            ],
            "readonly is applied as nowriteback",
        ),
        (
            ImageKind::CdRom,
            "writeback",
            vec!["ScsiAttachCdrom(id=next free)", "ScsiLoadMedia(id, \"x\")"],
            "writeback ignored",
        ),
    ] {
        let plan = plan_for(kind, flags)?;
        ensure!(
            plan.commands == commands && plan.notes.iter().any(|item| item.contains(note)),
            "snow plan for {} {flags:?}: {plan:?}",
            kind.name()
        );
        // Every command has its rpc call, with the same Snow method name.
        ensure!(
            plan.rpc.len() == plan.commands.len()
                && plan
                    .rpc
                    .iter()
                    .zip(&plan.commands)
                    .all(|(call, command)| command.starts_with(&format!("{}(", call.method))),
            "snow rpc calls for {} {flags:?} do not match the commands: {plan:?}",
            kind.name()
        );
    }
    for (text, needle) in [
        ("automount = true\nimage = \"Disks/Gone.hda\"\n", "Gone.hda"),
        (
            "automount = true\nimage = \"notes.txt\"\n",
            "not a supported disk image",
        ),
        ("automount = true\nimage = \"Broken.cue\"\n", "Lost.bin"),
        ("automount = maybe\n", "invalid hail.cfg"),
    ] {
        let actual = mounted(plan(Some(text))?).join(",");
        ensure!(
            actual.starts_with("skip: ") && actual.contains(needle),
            "mount plan for {text:?}: {actual}"
        );
    }
    let empty = context.root.join("empty-volume");
    fs::create_dir(&empty)?;
    fs::write(empty.join("hail.cfg"), "automount = true\n")?;
    ensure!(
        tracked(&empty).mount_plan() == MountPlan::Skip("no disk images in the volume root".into()),
        "empty volume plan wrong"
    );

    fs::write(
        &config_file,
        "automount = true\nselect = \"all\"\nmode = \"nowriteback\"\n",
    )?;
    let directory = context.root.join("automount");
    fs::create_dir(&directory)?;
    let config = Config::parse(&config_with(
        r#"
[[events."usb.inserted"]]
action = "log.write"
message = "in {label} cfg={hail_cfg} images={images}"
[[events."usb.inserted"]]
action = "emulator.automount"
[[events."usb.removed"]]
action = "log.write"
message = "out {label}"
"#,
    ))?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    let volume_disk = Disk {
        device: "dev-a".into(),
        label: "volume".into(),
        mount_point: volume.clone(),
    };
    let bare_disk = Disk {
        device: "dev-b".into(),
        label: "bare".into(),
        mount_point: context.root.join("bare-volume"),
    };
    fs::create_dir(&bare_disk.mount_point)?;
    let before = listing(&volume)?;
    service.observe(Ok(vec![volume_disk.clone(), bare_disk.clone()]))?;
    ensure!(listing(&volume)? == before, "automount modified the volume");
    ensure!(
        service.disks().len() == 2
            && service.disks()[0].disk == volume_disk
            && service.disks()[0].scan.images.len() == 4
            && service.disks()[1].scan.config == DiskConfigStatus::Absent,
        "inserted disks not tracked: {:?}",
        service.disks()
    );
    service.observe(Ok(vec![bare_disk.clone()]))?;
    ensure!(
        service.disks().len() == 1 && service.disks()[0].disk == bare_disk,
        "removed disk still tracked"
    );
    let path = service.log.path.clone();
    drop(service);
    let contents = fs::read_to_string(&path)?;
    let mounts = contents.matches("[debug] emulator.mount (stub").count();
    for needle in [
        "in volume cfg=found images=4\n".to_string(),
        "in bare cfg=absent images=0\n".to_string(),
        format!(
            "image: path={} kind=cd-rom size=4096 bytes",
            volume.join("Zeta.iso").display()
        ),
        format!(
            "image: path={} kind=floppy size=819200 bytes",
            volume.join("alpha.img").display()
        ),
        format!(
            "file: BINARY {} (2352 bytes)",
            volume.join("Game.bin").display()
        ),
        "track 01: MODE1/2352".to_string(),
        format!(
            "disk: device=dev-a label=volume mount_point={}",
            volume.display()
        ),
        format!(
            "hail.cfg: {} (automount=true emulator=- image=- select=all mode=nowriteback savefile=-)",
            config_file.display()
        ),
        "    | select = \"all\"".to_string(),
        "  mode: nowriteback\n".to_string(),
        format!(
            "  snow: ScsiAttachHdd(id=next free, \"{}\")\n  snow: ScsiBranchHdd(id, <host temporary copy>)\n  rpc: ScsiAttachHdd {} (not sent: stub)\n  rpc: ScsiBranchHdd {{\"id\":\"same\",\"path\":\"<host temporary copy>\"}} (not sent: stub)\n  note: guest writes go to a host-side copy",
            volume.join("Big.dsk").display(),
            serde_json::json!({"id": "next-free", "path": volume.join("Big.dsk").display().to_string()})
        ),
        format!(
            "  snow: ScsiAttachCdrom(id=next free)\n  snow: ScsiLoadMedia(id, \"{}\")\n  rpc: ScsiAttachCdrom {{\"id\":\"next-free\"}} (not sent: stub)\n  rpc: ScsiLoadMedia {} (not sent: stub)",
            volume.join("Zeta.iso").display(),
            serde_json::json!({"id": "same", "path": volume.join("Zeta.iso").display().to_string()})
        ),
        "modified=2003-".to_string(),
        "nothing to mount (no hail.cfg on disk)".to_string(),
        "out volume\n".to_string(),
    ] {
        ensure!(
            contents.contains(&needle),
            "automount log lacks {needle:?}:\n{contents}"
        );
    }
    ensure!(
        mounts == 4,
        "expected 4 stub mount calls, got {mounts}:\n{contents}"
    );
    println!("PASS disk media: image/CD-ROM cue classification, hail.cfg, read-only scan, disk tracking, automount stub log");
    Ok(())
}

fn emulator_actions(context: &TestContext) -> Result<()> {
    let default = Config::load(&context.default_config)?;
    ensure!(
        default.emulator == Some(EmulatorKind::Snow),
        "default config must select the snow emulator"
    );
    ensure!(
        EmulatorKind::from_name("snow") == Some(EmulatorKind::Snow)
            && EmulatorKind::Snow.handler().kind() == EmulatorKind::Snow,
        "snow handler lookup wrong"
    );
    let no_emulator =
        |events: &str| config_with(events).replace("[emulator]\ntype = \"snow\"\n", "");
    let bare = Config::parse(&no_emulator(
        "[[events.\"usb.inserted\"]]\naction = \"log.write\"\nmessage = \"x\"\n",
    ))?;
    ensure!(bare.emulator.is_none(), "[emulator] must be optional");
    for action in ["emulator.start", "emulator.stop", "emulator.automount"] {
        expect_invalid(
            &no_emulator(&format!(
                "[[events.\"usb.inserted\"]]\naction = \"{action}\"\n"
            )),
            &format!("{action} requires an [emulator] section"),
        )?;
    }
    expect_invalid(
        &config_with("").replace("\"snow\"", "\"mame\""),
        "unknown emulator.type \"mame\"; supported: snow",
    )?;
    expect_invalid(
        &config_with("").replace("type = \"snow\"\n", "type = \"snow\"\npath = \"x\"\n"),
        "path",
    )?;
    expect_invalid(&config_with("").replace("type = \"snow\"\n", ""), "type")?;
    expect_invalid(
        &config_with("[[events.\"usb.removed\"]]\naction = \"emulator.stop\"\nforce = true\n"),
        "force",
    )?;

    let directory = context.root.join("emulator");
    fs::create_dir(&directory)?;
    let config = Config::parse(&config_with(
        r#"
[[events."service.started"]]
action = "log.write"
message = "up {emulator}"
[[events."service.started"]]
action = "emulator.start"
[[events."usb.inserted"]]
action = "emulator.start"
[[events."usb.removed"]]
action = "emulator.stop"
[[events."polling.failed"]]
action = "emulator.stop"
"#,
    ))?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    let running = |service: &Service| service.emulator().map(|emulator| emulator.is_running());
    ensure!(
        running(&service) == Some(false),
        "emulator must start stopped"
    );
    service.start(Path::new("emulator.toml"))?;
    ensure!(
        running(&service) == Some(true),
        "emulator.start did not start"
    );
    service.observe(Ok(vec![disk(&context.root, "emu")]))?;
    ensure!(
        running(&service) == Some(true),
        "second start changed state"
    );
    service.observe(Ok(Vec::new()))?;
    ensure!(
        running(&service) == Some(false),
        "emulator.stop did not stop"
    );
    service.observe(Err(anyhow::anyhow!("boom")))?;
    ensure!(
        running(&service) == Some(false),
        "second stop changed state"
    );
    let path = service.log.path.clone();
    drop(service);
    let contents = fs::read_to_string(&path)?;
    let entry = |call: &str, running: bool, trigger: &str, result: &str| {
        format!("[debug] emulator.{call} (stub: not yet connected to Snow)\n  emulator: snow running={running}\n  trigger: {trigger}\n  result: {result}\n")
    };
    let expected = [
        "up snow\n".to_string(),
        entry("start", false, "service.started", "started").replace(
            "  result: started",
            "  rpc: Run {} (not sent: stub)\n  result: started",
        ),
        entry("start", true, "usb.inserted", "already running, ignored"),
        entry("stop", true, "usb.removed", "stopped").replace(
            "  result: stopped",
            "  rpc: Quit {} (not sent: stub)\n  result: stopped",
        ),
        entry("stop", false, "polling.failed", "not running, ignored"),
    ];
    let mut rest = contents.as_str();
    for needle in &expected {
        let at = rest
            .find(needle.as_str())
            .with_context(|| format!("emulator log lacks (in order) {needle:?}:\n{contents}"))?;
        rest = &rest[at + needle.len()..];
    }
    println!("PASS emulator: [emulator] type selection/validation, snow handler, emulator.start/stop state, stub log and rpc lines");
    Ok(())
}

fn exit_actions(context: &TestContext) -> Result<()> {
    ensure!(
        Exit::Quit.code() == 0 && Exit::Restart.code() == EXIT_RESTART && EXIT_RESTART == 75,
        "exit codes changed"
    );
    let directory = context.root.join("exits");
    fs::create_dir(&directory)?;
    let config = Config::parse(&config_with(
        r#"
[[events."usb.inserted"]]
action = "log.write"
message = "in {label}"
[[events."usb.inserted"]]
action = "hail.restart"
[[events."usb.inserted"]]
action = "log.write"
message = "never"
[[events."usb.removed"]]
action = "log.write"
message = "never"
"#,
    ))?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    let events = service.observe(Ok(vec![
        disk(&context.root, "one"),
        disk(&context.root, "two"),
    ]))?;
    ensure!(
        events.len() == 1 && service.exit_requested() == Some(Exit::Restart),
        "hail.restart did not stop processing: {events:?}"
    );
    ensure!(
        service.observe(Ok(Vec::new()))?.is_empty(),
        "events dispatched after an exit request"
    );
    ensure!(
        service.run_interval(|| anyhow::bail!("scanned after exit"), |_| {})? == Exit::Restart,
        "run_interval ignored the exit request"
    );
    let path = service.log.path.clone();
    drop(service);
    let contents = fs::read_to_string(&path)?;
    ensure!(
        contents == "in one\n",
        "chain continued after hail.restart:\n{contents}"
    );

    let config = Config::parse(
        &config_with("[[events.\"usb.inserted\"]]\naction = \"hail.quit\"\n").replace(
            "mode = \"interval\"\ninterval_ms = 250\n",
            "mode = \"os-events\"\nsettle_ms = 50\n",
        ),
    )?;
    let log = EventLog::open(&directory, &directory, "quit.log", &[])?;
    let mut service = Service::new(config, log);
    let mut source = ScriptedSource {
        script: VecDeque::from([true, false]),
        timeouts: Vec::new(),
    };
    let exit = service.run_os_events(
        &mut source,
        || Ok(vec![disk(&context.root, "quit")]),
        |_| {},
    )?;
    ensure!(
        exit == Exit::Quit && source.script.is_empty(),
        "run_os_events did not return hail.quit"
    );
    println!("PASS hail.quit/hail.restart stop chains and run loops with exit codes 0/75");
    Ok(())
}

/// An absolute path that is valid on the host OS.
fn absolute_example(name: &str) -> String {
    if cfg!(windows) {
        format!("C:\\\\Emulators\\\\{name}.exe")
    } else {
        format!("/opt/emulators/{name}")
    }
}

fn emulators_and_reload(context: &TestContext) -> Result<()> {
    let default = Config::load(&context.default_config)?;
    let names: Vec<&str> = default
        .emulators
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    ensure!(
        names.len() >= 30
            && default.emulators.configured() == 0
            && ["Snow", "NES", "SNES", "N64", "Genesis", "PS1", "MAME", "3DO"]
                .iter()
                .all(|name| names.contains(name)),
        "default [emulators] list incomplete: {names:?}"
    );
    let text = fs::read_to_string(&context.default_config)?;
    let section = text
        .split("\n[emulators]\n")
        .nth(1)
        .context("default config has no [emulators] section")?;
    let first = section
        .lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or_default();
    ensure!(
        first.starts_with("Snow = \"\""),
        "Snow must be the first [emulators] entry, got {first:?}"
    );

    let snes = absolute_example("snes9x");
    let config = Config::parse(&config_with(&format!(
        "[emulators]\nSNES = \"{snes}\"\nGenesis = \"\"\n\"3DO\" = \"  \"\n"
    )))?;
    let entry = config.emulators.get("snes").context("SNES not found")?;
    ensure!(
        entry.name == "SNES"
            && entry.path.as_deref() == Some(Path::new(&snes.replace("\\\\", "\\")))
            && config
                .emulators
                .get("GENESIS")
                .is_some_and(|entry| entry.path.is_none())
            && config
                .emulators
                .get("3do")
                .is_some_and(|entry| entry.path.is_none())
            && config.emulators.configured() == 1
            && config.emulators.get("NES").is_none(),
        "[emulators] parsed wrongly: {:?}",
        config.emulators
    );
    ensure!(
        Config::parse(&config_with(""))?
            .emulators
            .entries()
            .is_empty(),
        "[emulators] must be optional"
    );
    expect_invalid(
        &config_with("[emulators]\n\"bad name\" = \"\"\n"),
        "invalid short name",
    )?;
    expect_invalid(
        &config_with("[emulators]\nSNES = \"snes9x.exe\"\n"),
        "must be the full path",
    )?;
    expect_invalid(
        &config_with("[emulators]\nSnes = \"\"\nSNES = \"\"\n"),
        "are the same short name",
    )?;
    expect_invalid(&config_with("[emulators]\nSNES = 5\n"), "expected a string")?;

    let directory = context.root.join("emulators");
    let cfg_path = directory.join("hail.cfg");
    fs::create_dir(&directory)?;
    for (text, needle) in [
        ("automount = true\nemulator = \"SNES\"\n", "needs image"),
        (
            "automount = true\nemulator = \"no way\"\nimage = \"a.sfc\"\n",
            "not a valid [emulators] short name",
        ),
    ] {
        let error = DiskConfig::parse(cfg_path.clone(), text)
            .err()
            .with_context(|| format!("hail.cfg should be rejected: {text}"))?;
        ensure!(
            format!("{error:#}").contains(needle),
            "hail.cfg rejection {error:#} lacks {needle:?}"
        );
    }
    let parsed = DiskConfig::parse(
        cfg_path.clone(),
        "automount = true\nemulator = \"snow\"\nselect = \"first\"\n",
    )?;
    ensure!(
        parsed.emulator.as_deref() == Some("snow"),
        "hail.cfg emulator not kept"
    );

    // One disk per case: listed with a path, listed blank, unlisted, Snow by name.
    let mut disks = Vec::new();
    for (name, cfg) in [
        (
            "listed",
            "emulator = \"snes\"\nimage = \"games/chrono.sfc\"\n",
        ),
        ("blank", "emulator = \"Genesis\"\nimage = \"sonic.md\"\n"),
        ("unlisted", "emulator = \"N64\"\nimage = \"mario.z64\"\n"),
        ("missing", "emulator = \"SNES\"\nimage = \"gone.sfc\"\n"),
        ("broken", "emulator = \"GBA\"\nimage = \"zelda.gba\"\n"),
        ("snow", "emulator = \"Snow\"\nselect = \"first\"\n"),
    ] {
        let volume = directory.join(name);
        fs::create_dir_all(volume.join("games"))?;
        fs::write(
            volume.join("hail.cfg"),
            format!("automount = true\nmode = \"readonly\"\n{cfg}"),
        )?;
        disks.push(Disk {
            device: format!("dev-{name}"),
            label: name.to_string(),
            mount_point: volume,
        });
    }
    sized(
        &directory.join("listed").join("games").join("chrono.sfc"),
        4096,
    )?;
    sized(&directory.join("blank").join("sonic.md"), 512)?;
    sized(&directory.join("unlisted").join("mario.z64"), 512)?;
    sized(&directory.join("broken").join("zelda.gba"), 512)?;
    // The fake emulator is this test binary under another name (see `main`).
    let fake_dir = directory.join("fake emulator");
    fs::create_dir(&fake_dir)?;
    let fake = fake_dir.join(format!("{FAKE_EMULATOR}{}", std::env::consts::EXE_SUFFIX));
    fs::copy(std::env::current_exe()?, &fake)?;
    let fake_toml = fake.display().to_string().replace('\\', "\\\\");
    let gone = absolute_example("gba-emulator");
    sized(&directory.join("snow").join("system.dsk"), 20 * 1024 * 1024)?;
    let plan = tracked(&directory.join("listed")).mount_plan();
    ensure!(
        matches!(&plan, MountPlan::External { emulator, image, savefile: None, custom: None }
            if emulator == "snes" && image.ends_with(Path::new("games").join("chrono.sfc"))),
        "hail.cfg emulator did not plan an external launch: {plan:?}"
    );
    ensure!(
        matches!(tracked(&directory.join("snow")).mount_plan(), MountPlan::Mount(images) if images.len() == 1),
        "emulator = \"Snow\" must use the Snow handler"
    );
    ensure!(
        matches!(tracked(&directory.join("missing")).mount_plan(), MountPlan::Skip(reason) if reason.contains("gone.sfc")),
        "missing external image not skipped"
    );

    let config = Config::parse(&config_with(&format!(
        "[emulators]\nSNES = \"{fake_toml}\"\nGBA = \"{gone}\"\nGenesis = \"\"\n[[events.\"usb.inserted\"]]\naction = \"emulator.automount\"\n"
    )))?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    service.observe(Ok(disks))?;
    let path = service.log.path.clone();
    drop(service);
    let contents = fs::read_to_string(&path)?;
    for needle in [
        "[emulator] emulator.launch SNES started (pid ".to_string(),
        format!(
            "  command: \"{}\" \"{}\"",
            fake.display(),
            directory
                .join("listed")
                .join("games")
                .join("chrono.sfc")
                .display()
        ),
        format!(
            "[error] emulator.launch GBA failed to start {}",
            gone.replace("\\\\", "\\")
        ),
        format!(
            "  image: path={} size=4096 bytes",
            directory
                .join("listed")
                .join("games")
                .join("chrono.sfc")
                .display()
        ),
        "  mode: readonly (not applied: SNES only receives its [emulators] args)".to_string(),
        "  session: ".to_string(),
        "(automount=true emulator=snes image=games".to_string(),
        "mode=readonly savefile=-)".to_string(),
        "no executable set for Genesis in [emulators]".to_string(),
        "hail.cfg emulator \"N64\" is not listed in [emulators]".to_string(),
        "gone.sfc".to_string(),
        "[debug] emulator.mount (stub".to_string(),
    ] {
        ensure!(
            contents.contains(&needle),
            "external automount log lacks {needle:?}:\n{contents}"
        );
    }
    ensure!(
        contents.matches("emulator.launch").count() == 2,
        "expected one started and one failed external launch:\n{contents}"
    );
    let record = fake_dir.join(FAKE_EMULATOR_RECORD);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !record.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
    let launched = fs::read_to_string(&record).context("fake emulator was not started")?;
    let expected_image = directory.join("listed").join("games").join("chrono.sfc");
    ensure!(
        launched
            == format!(
                "cwd={}\narg={}\nHAIL_IMAGE={}\nHAIL_SAVEFILE=\n",
                fake_dir.display(),
                expected_image.display(),
                expected_image.display()
            ),
        "emulator not launched with the image (in its own folder):\n{launched}"
    );

    // Configuration reload: valid edits replace the running config, invalid ones are
    // rejected once and the running config is kept.
    let reload = context.root.join("reload");
    fs::create_dir(&reload)?;
    let config_file = reload.join("hail.toml");
    let first = config_with(
        "[[events.\"usb.inserted\"]]\naction = \"log.write\"\nmessage = \"one {label}\"\n",
    );
    fs::write(&config_file, &first)?;
    let config = Config::load(&config_file)?;
    let log = EventLog::open(&reload, &reload, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    service.watch_config(&config_file, Duration::from_millis(20))?;
    ensure!(
        service.check_config()? == ConfigReload::Unchanged,
        "unchanged file reported as changed"
    );
    let second = first
        .replace("one {label}", "two {label}")
        .replace("test.log.txt", "second.log")
        + &format!("[emulators]\nSNES = \"{snes}\"\n");
    fs::write(&config_file, &second)?;
    ensure!(
        service.check_config()? == ConfigReload::Reloaded
            && service.config.emulators.configured() == 1
            && service.log.path.ends_with("second.log"),
        "valid edit not applied"
    );
    service.observe(Ok(vec![disk(&reload, "a")]))?;
    fs::write(&config_file, second.replace("log.write", "log.nope"))?;
    let rejected = service.check_config()?;
    ensure!(
        matches!(&rejected, ConfigReload::Rejected(error) if error.contains("log.nope")),
        "invalid edit not rejected: {rejected:?}"
    );
    std::thread::sleep(Duration::from_millis(30));
    fs::write(&config_file, second.replace("log.write", "log.nope") + "\n")?;
    ensure!(
        matches!(service.check_config()?, ConfigReload::Rejected(_)),
        "edited invalid file not re-checked"
    );
    service.observe(Ok(vec![disk(&reload, "a"), disk(&reload, "b")]))?;
    let new_log = service.log.path.clone();
    drop(service);
    let old = fs::read_to_string(reload.join("test.log.txt")).unwrap_or_default();
    let new = fs::read_to_string(&new_log)?;
    ensure!(
        old.is_empty()
            && new.contains("[config] reloaded ")
            && new.contains("two a\n")
            && new.contains("two b\n")
            && new.matches("[config] rejected changes to ").count() == 2
            && new.contains("keeping the running configuration"),
        "reload log wrong:\nold:\n{old}\nnew:\n{new}"
    );

    // A reload that changes [polling] mode switches loops inside Service::run.
    let switch_file = reload.join("switch.toml");
    fs::write(&switch_file, config_with(""))?;
    let config = Config::load(&switch_file)?;
    let log = EventLog::open(&reload, &reload, "switch.log", &[])?;
    let mut service = Service::new(config, log);
    service.watch_config(&switch_file, Duration::from_millis(50))?;
    let switched = config_with("[[events.\"usb.inserted\"]]\naction = \"hail.quit\"\n")
        .replace(
            "mode = \"interval\"\ninterval_ms = 250\n",
            "mode = \"os-events\"\nsettle_ms = 50\n",
        )
        .replace("test.log.txt", "switch.log");
    let mut scans = 0;
    let mut subscribed = 0;
    let mut source: Option<Box<dyn ChangeSource>> = None;
    let started = Instant::now();
    let exit = service.run(
        &mut source,
        || {
            subscribed += 1;
            Ok(Box::new(ScriptedSource {
                script: VecDeque::new(),
                timeouts: Vec::new(),
            }) as Box<dyn ChangeSource>)
        },
        || {
            scans += 1;
            if scans == 1 {
                fs::write(&switch_file, &switched)?;
                Ok(vec![])
            } else {
                Ok(vec![disk(&reload, "switch")])
            }
        },
        |_| {},
    )?;
    ensure!(
        exit == Exit::Quit
            && subscribed == 1
            && source.is_some()
            && service.config.polling.mode.name() == "os-events",
        "mode switch via reload failed: exit={exit:?} subscribed={subscribed} scans={scans}"
    );
    ensure!(
        started.elapsed() < Duration::from_secs(5),
        "mode switch took too long"
    );
    drop(service);

    // The stop handle ends the run loops with Exit::Stop (code 0).
    let config = Config::parse(&config_with(""))?;
    let log = EventLog::open(&reload, &reload, "stop.log", &[])?;
    let mut service = Service::new(config, log);
    let stop = service.stop_handle();
    let setter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let exit = service.run_interval(|| Ok(vec![]), |_| {})?;
    setter.join().ok();
    ensure!(
        exit == Exit::Stop && exit.code() == 0 && started.elapsed() < Duration::from_secs(2),
        "stop handle did not stop the run loop: {exit:?} after {:?}",
        started.elapsed()
    );
    println!("PASS [emulators] registry, hail.cfg emulator launches, config reload accept/reject, mode switch on reload, stop handle");
    Ok(())
}

fn wait_for(path: &Path) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    fs::read_to_string(path).with_context(|| format!("{} was not written", path.display()))
}

fn savefiles_and_sessions(context: &TestContext) -> Result<()> {
    let root = context.root.join("savefiles");
    fs::create_dir(&root)?;
    let cfg_path = root.join("hail.cfg");

    // hail.cfg savefile: a writeback-only path on the disk.
    let parsed = DiskConfig::parse(
        cfg_path.clone(),
        "automount = true\nemulator = \"SNES\"\nimage = \"Chrono Trigger.sfc\"\nmode = \"writeback\"\nsavefile = \"Saves/Chrono Trigger.srm\"\n",
    )?;
    ensure!(
        parsed.savefile.as_deref() == Some(&Path::new("Saves").join("Chrono Trigger.srm")),
        "savefile not parsed: {:?}",
        parsed.savefile
    );
    for (text, needle) in [
        (
            "savefile = \"a.srm\"\n",
            "savefile requires mode = \"writeback\"",
        ),
        (
            "mode = \"nowriteback\"\nsavefile = \"a.srm\"\n",
            "savefile requires mode",
        ),
        (
            "mode = \"writeback\"\nselect = \"all\"\nsavefile = \"a.srm\"\n",
            "select = \"all\"",
        ),
        (
            "mode = \"writeback\"\nsavefile = \"../a.srm\"\n",
            "savefile \"../a.srm\" must stay on the disk",
        ),
        (
            "mode = \"writeback\"\nsavefile = \"C:/a.srm\"\n",
            "savefile \"C:/a.srm\" must be relative",
        ),
        (
            "mode = \"writeback\"\nsavefile = \"/\"\n",
            "savefile must name a file",
        ),
        (
            "mode = \"writeback\"\nimage = \"Game.hda\"\nsavefile = \"game.HDA\"\n",
            "savefile must differ from image",
        ),
    ] {
        let error = DiskConfig::parse(cfg_path.clone(), &format!("automount = true\n{text}"))
            .err()
            .with_context(|| format!("hail.cfg should be rejected: {text}"))?;
        ensure!(
            format!("{error:#}").contains(needle),
            "hail.cfg rejection {error:#} lacks {needle:?}"
        );
    }

    // Snow: hard disks branch to the savefile on first use and resume from it later.
    let saves = root.join("Saves");
    fs::create_dir(&saves)?;
    let savefile = saves.join("System.hda");
    let mut image = DiskImage {
        path: root.join("System.hda"),
        kind: ImageKind::HardDisk,
        size: 1,
        modified: None,
        created: None,
        cue: None,
    };
    let writeback = MountMode::parse("writeback")?;
    let first = SnowMount::plan(&image, writeback, Some(&savefile));
    ensure!(
        first.commands
            == vec![
                format!("ScsiAttachHdd(id=next free, \"{}\")", image.path.display()),
                format!("ScsiBranchHdd(id, \"{}\")", savefile.display()),
            ]
            && first.notes.iter().any(|note| note.contains("first use")),
        "first savefile mount wrong: {first:?}"
    );
    sized(&savefile, 512)?;
    let resumed = SnowMount::plan(&image, writeback, Some(&savefile));
    ensure!(
        resumed.commands
            == vec![format!(
                "ScsiAttachHdd(id=next free, \"{}\")",
                savefile.display()
            )]
            && resumed
                .notes
                .iter()
                .any(|note| note.contains("resuming from savefile")),
        "resumed savefile mount wrong: {resumed:?}"
    );
    image.kind = ImageKind::Floppy;
    let floppy = SnowMount::plan(&image, writeback, Some(&savefile));
    ensure!(
        floppy.commands.len() == 2 && floppy.notes.iter().any(|note| note.contains("not used")),
        "floppy savefile note missing: {floppy:?}"
    );
    image.kind = ImageKind::CdRom;
    let cdrom = SnowMount::plan(&image, writeback, Some(&savefile));
    ensure!(
        cdrom
            .notes
            .iter()
            .any(|note| note.contains("ignored: CD-ROM")),
        "CD-ROM savefile note missing: {cdrom:?}"
    );

    // [emulators] table form with argument templates.
    let retro = absolute_example("retroarch");
    let config = Config::parse(&config_with(&format!(
        "[emulators]\nSNES = {{ path = \"{retro}\", args = [\"-L\", \"cores/snes9x.dll\", \"--save={{savefile}}\", \"{{image}}\"] }}\nNES = \"{retro}\"\n"
    )))?;
    let snes = config.emulators.get("snes").context("SNES missing")?;
    let image_path = root.join("Chrono Trigger.sfc");
    let save_path = saves.join("Chrono Trigger.srm");
    let with_save = snes.command_args(&image_path, Some(&save_path));
    let without_save = snes.command_args(&image_path, None);
    ensure!(
        snes.uses_savefile()
            && with_save
                == vec![
                    "-L".to_string(),
                    "cores/snes9x.dll".to_string(),
                    format!("--save={}", save_path.display()),
                    image_path.display().to_string(),
                ]
            && without_save
                == vec![
                    "-L".to_string(),
                    "cores/snes9x.dll".to_string(),
                    image_path.display().to_string(),
                ],
        "SNES args rendered wrongly: {with_save:?} / {without_save:?}"
    );
    let nes = config.emulators.get("NES").context("NES missing")?;
    ensure!(
        !nes.uses_savefile()
            && nes.command_args(&image_path, Some(&save_path))
                == vec![image_path.display().to_string()],
        "default args must be just the image"
    );
    let request = hail::emulator::launch_request(snes, Path::new(&retro), &image_path, None);
    ensure!(
        request.env
            == vec![
                ("HAIL_IMAGE".to_string(), image_path.display().to_string()),
                ("HAIL_SAVEFILE".to_string(), String::new()),
            ],
        "launch environment wrong: {:?}",
        request.env
    );
    for (value, needle) in [
        (
            "{ path = \"\", args = [\"--save={savefile}\"] }",
            "must pass the image",
        ),
        (
            "{ path = \"\", args = [\"{rom}\"] }",
            "unknown placeholder {rom}",
        ),
        (
            "{ path = \"\", args = \"{image}\" }",
            "args must be an array",
        ),
        ("{ path = \"\", args = [5] }", "args[0] must be a string"),
        ("{ path = \"\", argv = [] }", "unknown key \"argv\""),
        ("{ args = [\"{image}\"] }", "SNES.path: expected a string"),
        ("5", "expected a string"),
    ] {
        expect_invalid(
            &config_with(&format!("[emulators]\nSNES = {value}\n")),
            needle,
        )?;
    }

    // [service] emulator_session.
    let with_session = |value: &str| {
        config_with("").replace(
            "log_file = \"test.log.txt\"\n",
            &format!("log_file = \"test.log.txt\"\nemulator_session = \"{value}\"\n"),
        )
    };
    ensure!(
        Config::parse(&config_with(""))?.emulator_session == EmulatorSession::User
            && Config::parse(&with_session("service"))?.emulator_session
                == EmulatorSession::Service
            && Config::parse(&with_session("user"))?.emulator_session == EmulatorSession::User,
        "emulator_session parsed wrongly"
    );
    expect_invalid(&with_session("desktop"), "unknown variant")?;

    // Windows command lines and environment blocks.
    for (argument, quoted) in [
        ("plain", "plain"),
        ("", "\"\""),
        ("two words", "\"two words\""),
        ("say \"hi\"", "\"say \\\"hi\\\"\""),
        ("C:\\Saves Dir\\", "\"C:\\Saves Dir\\\\\""),
        ("a\\\\\"b", "\"a\\\\\\\\\\\"b\""),
        ("C:\\no-space\\", "C:\\no-space\\"),
    ] {
        ensure!(
            quote_windows_arg(argument) == quoted,
            "quote_windows_arg({argument:?}) = {:?}, expected {quoted:?}",
            quote_windows_arg(argument)
        );
    }
    ensure!(
        windows_command_line(
            Path::new("C:\\Emu\\snes 9x.exe"),
            &["--save=C:\\a b.srm".to_string(), "x.sfc".to_string()]
        ) == "\"C:\\Emu\\snes 9x.exe\" \"--save=C:\\a b.srm\" x.sfc",
        "windows_command_line wrong"
    );
    let block = environment_block(&[
        ("=C:".to_string(), "C:\\".to_string()),
        ("Path".to_string(), "C:\\Windows".to_string()),
        ("USERNAME".to_string(), "ana".to_string()),
    ]);
    let parsed = parse_environment_block(&block);
    let merged = merge_environment(
        parsed.clone(),
        &[
            ("PATH".to_string(), "D:\\Emu".to_string()),
            ("HAIL_SAVEFILE".to_string(), String::new()),
        ],
    );
    ensure!(
        block.ends_with(&[0, 0])
            && parsed
                == vec![
                    ("=C:".to_string(), "C:\\".to_string()),
                    ("Path".to_string(), "C:\\Windows".to_string()),
                    ("USERNAME".to_string(), "ana".to_string()),
                ]
            && merged
                == vec![
                    ("=C:".to_string(), "C:\\".to_string()),
                    ("Path".to_string(), "D:\\Emu".to_string()),
                    ("USERNAME".to_string(), "ana".to_string()),
                    ("HAIL_SAVEFILE".to_string(), String::new()),
                ]
            && parse_environment_block(&environment_block(&[])).is_empty(),
        "environment block helpers wrong: {parsed:?} / {merged:?}"
    );

    // Linux: the active graphical logind session of a real user is chosen.
    let session = |id: &str, text: &str| LogindSession::parse(id, text);
    let sessions: Vec<LogindSession> = [
        session("1", "UID=0\nUSER=root\nACTIVE=1\nTYPE=x11\nCLASS=user\n"),
        session(
            "2",
            "UID=113\nUSER=lightdm\nACTIVE=1\nTYPE=x11\nCLASS=greeter\n",
        ),
        session(
            "3",
            "UID=1000\nUSER=pi\nACTIVE=0\nSTATE=online\nTYPE=wayland\nCLASS=user\n",
        ),
        session("4", "UID=1001\nUSER=ssh\nACTIVE=1\nTYPE=tty\nCLASS=user\n"),
        session(
            "c5",
            "# comment\nUID=1000\nUSER=pi\nACTIVE=1\nTYPE=x11\nCLASS=user\nDISPLAY=:1\n",
        ),
        session("7", "UID=1002\nUSER=remote\nSTATE=active\nTYPE=wayland\n"),
        session("8", "USER=broken\n"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let chosen = LogindSession::pick(&sessions);
    ensure!(
        sessions.len() == 6
            && chosen.is_some_and(|chosen| chosen.id == "7" && chosen.user == "remote"),
        "logind pick wrong without seats: {chosen:?}"
    );
    let mut seated = sessions.clone();
    seated[4].seat = Some("seat0".to_string());
    let chosen = LogindSession::pick(&seated);
    ensure!(
        chosen.is_some_and(|chosen| chosen.id == "c5"
            && chosen.uid == 1000
            && chosen.display.as_deref() == Some(":1")),
        "logind pick must prefer a seated session: {chosen:?}"
    );
    ensure!(
        LogindSession::pick(&sessions[..4]).is_none(),
        "root, greeter, inactive and tty sessions must not be picked"
    );

    // A launch with a savefile: args, environment, log.
    let fake_dir = root.join("fake emulator");
    fs::create_dir(&fake_dir)?;
    let fake = fake_dir.join(format!("{FAKE_EMULATOR}{}", std::env::consts::EXE_SUFFIX));
    fs::copy(std::env::current_exe()?, &fake)?;
    let fake_toml = fake.display().to_string().replace('\\', "\\\\");
    let volume = root.join("cart");
    fs::create_dir_all(volume.join("Saves"))?;
    sized(&volume.join("Chrono Trigger.sfc"), 4096)?;
    fs::write(
        volume.join("hail.cfg"),
        "automount = true\nemulator = \"SNES\"\nimage = \"Chrono Trigger.sfc\"\nmode = \"writeback\"\nsavefile = \"Saves/Chrono Trigger.srm\"\n",
    )?;
    let tracked_cart = tracked(&volume);
    let expected_save = volume.join("Saves").join("Chrono Trigger.srm");
    ensure!(
        tracked_cart.savefile().as_deref() == Some(expected_save.as_path())
            && matches!(tracked_cart.mount_plan(), MountPlan::External { savefile: Some(path), .. } if path == expected_save),
        "savefile not planned: {:?}",
        tracked_cart.mount_plan()
    );
    let config = Config::parse(&config_with(&format!(
        "[emulators]\nSNES = {{ path = \"{fake_toml}\", args = [\"--save={{savefile}}\", \"{{image}}\"] }}\n[[events.\"usb.inserted\"]]\naction = \"emulator.automount\"\n"
    )))?;
    let log = EventLog::open(&root, &root, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    service.observe(Ok(vec![Disk {
        device: "dev-cart".into(),
        label: "cart".into(),
        mount_point: volume.clone(),
    }]))?;
    let path = service.log.path.clone();
    drop(service);
    let contents = fs::read_to_string(&path)?;
    let image_path = volume.join("Chrono Trigger.sfc");
    for needle in [
        "[emulator] emulator.launch SNES started (pid ".to_string(),
        format!(
            "  command: \"{}\" \"--save={}\" \"{}\"",
            fake.display(),
            expected_save.display(),
            image_path.display()
        ),
        format!(
            "  savefile: {} (new, passed in args and HAIL_SAVEFILE)",
            expected_save.display()
        ),
        "  mode: writeback (applied: SNES saves to the savefile)".to_string(),
        "  session: ".to_string(),
        "mode=writeback savefile=Saves".to_string(),
    ] {
        ensure!(
            contents.contains(&needle),
            "savefile launch log lacks {needle:?}:\n{contents}"
        );
    }
    let launched = wait_for(&fake_dir.join(FAKE_EMULATOR_RECORD))?;
    let expected = format!(
        "cwd={}\narg=--save={}\narg={}\nHAIL_IMAGE={}\nHAIL_SAVEFILE={}\n",
        fake_dir.display(),
        expected_save.display(),
        image_path.display(),
        image_path.display(),
        expected_save.display()
    );
    ensure!(
        launched == expected,
        "fake emulator saw wrong args/environment:\n{launched}\nexpected:\n{expected}"
    );
    println!("PASS hail.cfg savefile, Snow savefile branch/resume, [emulators] args templates, emulator_session, Windows quoting/env block, logind session pick, savefile launch");
    Ok(())
}

fn custom_emulators(context: &TestContext) -> Result<()> {
    let root = context.root.join("custom");
    fs::create_dir(&root)?;
    let cfg_path = root.join("hail.cfg");
    let exe = format!("{FAKE_EMULATOR}{}", std::env::consts::EXE_SUFFIX);

    // hail.cfg emulator as a table.
    let named = DiskConfig::parse(
        cfg_path.clone(),
        "emulator.name = \"SNES\"\nimage = \"a.sfc\"\n",
    )?;
    ensure!(
        named.emulator.as_deref() == Some("SNES") && named.custom.is_none(),
        "emulator.name without custom must name an [emulators] entry: {named:?}"
    );
    let on_disk = DiskConfig::parse(
        cfg_path.clone(),
        "automount = true\nimage = \"game.rom\"\nemulator.custom = true\nemulator.name = \"MyEmu\"\nemulator.path = \"Emulator/myemu.exe\"\n",
    )?;
    ensure!(
        on_disk.emulator.as_deref() == Some("MyEmu")
            && on_disk.custom.as_ref().is_some_and(|custom| custom.path
                == PerPlatform::Any(CustomPath::Disk(Path::new("Emulator").join("myemu.exe")))
                && custom.args.len() == 1
                && custom.args[0].uses("image")),
        "disk custom emulator parsed wrongly: {on_disk:?}"
    );
    let host_path = absolute_example("myemu").replace("\\\\", "\\");
    let on_host = DiskConfig::parse(
        cfg_path.clone(),
        &format!("image = \"game.rom\"\n[emulator]\ncustom = true\nname = \"MyEmu\"\npath = '{host_path}'\nargs = [\"-f\", \"--rom={{image}}\"]\n"),
    )?;
    ensure!(
        on_host.custom.as_ref().is_some_and(|custom| custom.path
            == PerPlatform::Any(CustomPath::Host(PathBuf::from(&host_path)))
            && custom.args.len() == 2),
        "host custom emulator parsed wrongly: {on_host:?}"
    );
    for (text, needle) in [
        (
            "image = \"a\"\nemulator.name = \"X\"\nemulator.path = \"x.exe\"\n",
            "emulator.path needs emulator.custom = true",
        ),
        (
            "image = \"a\"\nemulator.name = \"X\"\nemulator.args = [\"{image}\"]\n",
            "emulator.args needs emulator.custom = true",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\n",
            "needs emulator.path",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path = \" \"\n",
            "needs emulator.path",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path = \"../x.exe\"\n",
            "must stay on the disk",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path = '\\emu\\x.exe'\n",
            "must be a full host path or relative to the disk root",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.path = \"x.exe\"\n",
            "emulator.name is required",
        ),
        (
            "image = \"a\"\nemulator.custom = \"yes\"\nemulator.name = \"X\"\n",
            "emulator.custom must be true or false",
        ),
        (
            "image = \"a\"\nemulator.name = \"X\"\nemulator.argv = []\n",
            "unknown key \"argv\"",
        ),
        (
            "image = \"a\"\nemulator.name = \"bad name\"\n",
            "not a valid [emulators] short name",
        ),
        (
            "emulator.custom = true\nemulator.name = \"X\"\nemulator.path = \"x.exe\"\n",
            "custom emulator \"X\" needs image",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path = \"x.exe\"\nemulator.args = [\"-f\"]\n",
            "emulator.args must pass the image",
        ),
        ("image = \"a\"\nemulator = 5\n", "emulator must be a short name or a table"),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path.macos.x64 = \"x\"\n",
            "emulator.path: unknown OS \"macos\"",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path.windows.ia64 = \"x\"\n",
            "emulator.path.windows: unknown architecture \"ia64\"",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path.linux.x64 = '\\x'\n",
            "emulator.path.linux.x64 \"\\\\x\" must be a full host path or relative to the disk root",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path.windows.x64 = \"../x.exe\"\n",
            "must stay on the disk",
        ),
        (
            "image = \"a\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path.linux.arm64 = 1\n",
            "emulator.path.linux.arm64 must be a string",
        ),
    ] {
        let error = DiskConfig::parse(cfg_path.clone(), text)
            .err()
            .with_context(|| format!("hail.cfg should be rejected: {text}"))?;
        ensure!(
            format!("{error:#}").contains(needle),
            "hail.cfg rejection {error:#} lacks {needle:?}"
        );
    }

    // [service] custom_emulators.
    let with_policy = |value: &str| {
        config_with("").replace(
            "log_file = \"test.log.txt\"\n",
            &format!("log_file = \"test.log.txt\"\ncustom_emulators = \"{value}\"\n"),
        )
    };
    ensure!(
        Config::parse(&config_with(""))?.custom_emulators == CustomEmulators::Defined,
        "custom_emulators must default to \"defined\""
    );
    for policy in CustomEmulators::ALL {
        ensure!(
            Config::parse(&with_policy(policy.name()))?.custom_emulators == policy,
            "custom_emulators = {:?} parsed wrongly",
            policy.name()
        );
    }
    expect_invalid(&with_policy("maybe"), "custom_emulators")?;

    // Policy checks against resolved paths.
    let host_dir = root.join("host emulators");
    fs::create_dir(&host_dir)?;
    let host_exe = host_dir.join(&exe);
    fs::copy(std::env::current_exe()?, &host_exe)?;
    let volume = root.join("stick");
    fs::create_dir_all(volume.join("Emulator"))?;
    let disk_exe = volume.join("Emulator").join(&exe);
    fs::copy(std::env::current_exe()?, &disk_exe)?;
    let host_toml = host_exe.display().to_string().replace('\\', "\\\\");
    let listed = Config::parse(&config_with(&format!(
        "[emulators]\nMyEmu = \"{host_toml}\"\n"
    )))?;
    let emulators = &listed.emulators;
    let check = |policy: CustomEmulators, exe: &Path| policy.check(exe, &volume, emulators);
    let refused = |result: std::result::Result<String, String>, needle: &str| matches!(result, Err(reason) if reason.contains(needle));
    ensure!(
        refused(
            check(CustomEmulators::None, &host_exe),
            "allows no hail.cfg custom emulators"
        ) && check(CustomEmulators::Any, &host_exe).is_ok()
            && check(CustomEmulators::Any, &disk_exe).is_ok()
            && check(CustomEmulators::Defined, &host_exe)
                .is_ok_and(|reason| reason.contains("[emulators] MyEmu"))
            && refused(
                check(CustomEmulators::Defined, &disk_exe),
                "is not an [emulators] executable"
            )
            && check(CustomEmulators::Disk, &disk_exe)
                .is_ok_and(|reason| reason.contains("on the inserted disk"))
            && check(
                CustomEmulators::Disk,
                &volume.join("Emulator").join(".").join(&exe)
            )
            .is_ok()
            && refused(
                check(CustomEmulators::Disk, &host_exe),
                "is not on the inserted disk"
            )
            && refused(
                check(CustomEmulators::Disk, &volume.join("gone.exe")),
                "cannot resolve"
            )
            && refused(
                check(CustomEmulators::Defined, &volume.join("gone.exe")),
                "cannot resolve"
            ),
        "custom_emulators checks wrong"
    );

    // Real launches: a disk-provided emulator, the same disk refused, a listed
    // executable with the disk's own args.
    sized(&volume.join("game.rom"), 2048)?;
    let image_path = volume.join("game.rom");
    let run = |policy: &str, cfg: &str, extra: &str| -> Result<String> {
        fs::write(volume.join("hail.cfg"), cfg)?;
        let config = Config::parse(&format!(
            "{}[emulators]\n{extra}[[events.\"usb.inserted\"]]\naction = \"emulator.automount\"\n",
            with_policy(policy)
        ))?;
        let log_dir = root.join(format!("log-{policy}"));
        fs::create_dir_all(&log_dir)?;
        let log = EventLog::open(&log_dir, &log_dir, &config.log_file, &[])?;
        let mut service = Service::new(config, log);
        service.observe(Ok(vec![Disk {
            device: "dev-stick".into(),
            label: "stick".into(),
            mount_point: volume.clone(),
        }]))?;
        let path = service.log.path.clone();
        drop(service);
        Ok(fs::read_to_string(path)?)
    };
    let disk_cfg = format!(
        "automount = true\nimage = \"game.rom\"\nmode = \"readonly\"\nemulator.custom = true\nemulator.name = \"MyEmu\"\nemulator.path = \"Emulator/{exe}\"\nemulator.args = [\"--rom\", \"{{image}}\"]\n"
    );
    let contents = run("disk", &disk_cfg, "")?;
    for needle in [
        "[emulator] emulator.launch MyEmu started (pid ".to_string(),
        "  emulator: custom, defined by hail.cfg (permitted by custom_emulators = \"disk\": executable is on the inserted disk)".to_string(),
        format!(
            "  command: \"{}\" \"--rom\" \"{}\"",
            disk_exe.display(),
            image_path.display()
        ),
        "  mode: readonly (not applied: MyEmu only receives its hail.cfg emulator.args)".to_string(),
        "emulator=MyEmu custom=disk:Emulator".to_string(),
    ] {
        ensure!(
            contents.contains(&needle),
            "disk custom emulator log lacks {needle:?}:\n{contents}"
        );
    }
    let launched = wait_for(&volume.join("Emulator").join(FAKE_EMULATOR_RECORD))?;
    let expected = format!(
        "cwd={}\narg=--rom\narg={}\nHAIL_IMAGE={}\nHAIL_SAVEFILE=\n",
        volume.join("Emulator").display(),
        image_path.display(),
        image_path.display()
    );
    ensure!(
        launched == expected,
        "disk emulator saw wrong args/environment:\n{launched}\nexpected:\n{expected}"
    );

    let contents = run("defined", &disk_cfg, &format!("MyEmu = \"{host_toml}\"\n"))?;
    ensure!(
        contents.contains(
            "[emulator] emulator.launch MyEmu refused: custom_emulators = \"defined\" and"
        ) && contents.contains(&format!("  executable: {}", disk_exe.display()))
            && !contents.contains("started (pid"),
        "disk emulator must be refused under \"defined\":\n{contents}"
    );

    let host_cfg = format!(
        "automount = true\nimage = \"game.rom\"\nemulator.custom = true\nemulator.name = \"Tuned\"\nemulator.path = '{}'\nemulator.args = [\"--fullscreen\", \"{{image}}\"]\n",
        host_exe.display()
    );
    let contents = run("any", &host_cfg, &format!("Other = \"{host_toml}\"\n"))?;
    ensure!(
        contents.contains("[emulator] emulator.launch Tuned started (pid ")
            && contents.contains("(permitted by custom_emulators = \"any\")")
            && contents.contains("custom=host:"),
        "host custom emulator not started under \"any\":\n{contents}"
    );
    let launched = wait_for(&host_dir.join(FAKE_EMULATOR_RECORD))?;
    ensure!(
        launched.contains("arg=--fullscreen\n"),
        "host custom emulator saw wrong args:\n{launched}"
    );
    let contents = run("none", &host_cfg, &format!("Tuned = \"{host_toml}\"\n"))?;
    ensure!(
        contents.contains("emulator.launch Tuned refused: custom_emulators = \"none\""),
        "custom_emulators = \"none\" must refuse:\n{contents}"
    );

    // emulator.path.<os>.<arch>: one disk carrying emulators for several hosts. Each
    // entry is a disk or host path for its own OS, whatever the host is.
    let (os, arch) = (hail::platform::host_os(), hail::platform::host_arch());
    let (other_os, other_path) = if cfg!(windows) {
        ("linux", "/opt/multi/multi")
    } else {
        ("windows", "C:\\\\Multi\\\\multi.exe")
    };
    let multi_cfg = format!(
        "automount = true\nimage = \"game.rom\"\nemulator.custom = true\nemulator.name = \"Multi\"\nemulator.path.{os}.{arch} = \"Emulator/{exe}\"\nemulator.path.{other_os}.arm64 = \"{other_path}\"\nemulator.path.{other_os}.x64 = \"Emulator/other/multi\"\nemulator.args = [\"--rom\", \"{{image}}\"]\n"
    );
    let parsed = DiskConfig::parse(cfg_path.clone(), &multi_cfg)?;
    let expected = PerPlatform::Platforms(BTreeMap::from([
        (
            format!("{os}.{arch}"),
            CustomPath::Disk(Path::new("Emulator").join(&exe)),
        ),
        (
            format!("{other_os}.arm64"),
            CustomPath::Host(PathBuf::from(other_path.replace("\\\\", "\\"))),
        ),
        (
            format!("{other_os}.x64"),
            CustomPath::Disk(Path::new("Emulator").join("other").join("multi")),
        ),
    ]));
    ensure!(
        parsed
            .custom
            .as_ref()
            .is_some_and(|custom| custom.path == expected
                && custom.path.for_host()
                    == Ok(&CustomPath::Disk(Path::new("Emulator").join(&exe)))),
        "per-platform emulator.path parsed wrongly: {parsed:?}"
    );
    let os_only = DiskConfig::parse(
        cfg_path.clone(),
        &format!("image = \"game.rom\"\nemulator.custom = true\nemulator.name = \"X\"\nemulator.path.{os} = \"Emulator/{exe}\"\n"),
    )?;
    ensure!(
        os_only
            .custom
            .as_ref()
            .is_some_and(|custom| custom.path.for_host()
                == Ok(&CustomPath::Disk(Path::new("Emulator").join(&exe)))),
        "emulator.path.<os> must apply to every architecture: {os_only:?}"
    );
    // Lookup order on any host: <os>.<arch>, <os>, then <os>.x64 only on an Arm host
    // that runs x64 programs (Windows on Arm with x64 emulation).
    let entries = |keys: &[&str]| {
        PerPlatform::Platforms(
            keys.iter()
                .map(|key| (key.to_string(), key.to_string()))
                .collect(),
        )
    };
    let pick = |value: &PerPlatform<String>, os: &str, arch: &str, runs_x64: bool| {
        value.for_platform(os, arch, runs_x64).cloned()
    };
    let all = entries(&["windows.arm64", "windows", "windows.x64"]);
    let no_arm = entries(&["windows", "windows.x64"]);
    let x64_only = entries(&["windows.x64", "linux.x64"]);
    for (value, os, arch, runs_x64, expected) in [
        (&all, "windows", "arm64", true, Ok("windows.arm64")),
        (&no_arm, "windows", "arm64", true, Ok("windows")),
        (&x64_only, "windows", "arm64", true, Ok("windows.x64")),
        (&all, "windows", "x64", false, Ok("windows.x64")),
        (&no_arm, "windows", "x64", false, Ok("windows.x64")),
        (
            &x64_only,
            "windows",
            "arm64",
            false,
            Err("no entry for this host (windows.arm64; looked for windows.arm64, windows); defined for linux.x64, windows.x64"),
        ),
        (
            &x64_only,
            "linux",
            "arm64",
            false,
            Err("no entry for this host (linux.arm64; looked for linux.arm64, linux); defined for linux.x64, windows.x64"),
        ),
    ] {
        let got = pick(value, os, arch, runs_x64);
        ensure!(
            got == expected.map(str::to_string).map_err(str::to_string),
            "for_platform({os}, {arch}, runs_x64={runs_x64}) = {got:?}, expected {expected:?}"
        );
    }
    ensure!(
        hail::platform::candidates("windows", "arm64", true)
            == ["windows.arm64", "windows", "windows.x64"]
            && hail::platform::candidates("windows", "x64", true) == ["windows.x64", "windows"]
            && hail::platform::candidates("linux", "arm64", false) == ["linux.arm64", "linux"],
        "platform candidates wrong"
    );
    ensure!(
        hail::platform::ARCHES.contains(&hail::platform::host_arch()),
        "host_arch {} is not a known architecture",
        hail::platform::host_arch()
    );
    let contents = run("disk", &multi_cfg, "")?;
    ensure!(
        contents.contains("[emulator] emulator.launch Multi started (pid ")
            && contents.contains(&format!(
                "  command: \"{}\" \"--rom\" \"{}\"",
                disk_exe.display(),
                image_path.display()
            ))
            && contents.contains(&format!(" {os}.{arch}=disk:Emulator")),
        "the emulator.path entry for this host must be launched:\n{contents}"
    );
    let away_cfg = format!(
        "automount = true\nimage = \"game.rom\"\nemulator.custom = true\nemulator.name = \"Away\"\nemulator.path.{other_os}.x64 = \"Emulator/other/multi\"\n"
    );
    let contents = run("any", &away_cfg, "")?;
    ensure!(
        contents.contains(&format!(
            "[emulator] emulator.launch Away skipped: emulator.path: no entry for this host ({os}.{arch}; looked for {}); defined for {other_os}.x64",
            hail::platform::candidates(os, arch, hail::platform::host_runs_x64()).join(", ")
        )) && !contents.contains("emulator.launch Away started"),
        "a hail.cfg without an emulator.path for this host must be skipped and logged:\n{contents}"
    );
    println!("PASS hail.cfg custom emulators (table form, disk/host paths, per-platform emulator.path.<os>.<arch> with the Windows on Arm x64 fallback, args), custom_emulators none/defined/disk/any, disk-provided and host launches, refusals, no entry for this host");
    Ok(())
}

fn run_app(app: &Path, arguments: &[&str]) -> Result<Output> {
    Command::new(app)
        .args(arguments)
        .output()
        .with_context(|| format!("running {} {arguments:?}", app.display()))
}

fn executable(context: &TestContext) -> Result<()> {
    let install = context.root.join("install");
    fs::create_dir(&install)?;
    let app = install.join(context.app.file_name().context("app has no file name")?);
    fs::copy(&context.app, &app)?;
    fs::copy(&context.default_config, install.join("hail.toml"))?;

    let help = run_app(&app, &["--help"])?;
    ensure!(
        help.status.success() && String::from_utf8(help.stdout)?.contains("--check-config"),
        "--help failed"
    );
    let version = run_app(&app, &["--version"])?;
    ensure!(
        version.status.success()
            && String::from_utf8(version.stdout)?.trim() == format!("hail {}", hail::VERSION),
        "--version failed"
    );
    let invalid = run_app(&app, &["--bogus"])?;
    ensure!(
        !invalid.status.success() && !invalid.stderr.is_empty(),
        "invalid argument did not fail"
    );
    let check = run_app(&app, &["--check-config"])?;
    ensure!(
        check.status.success(),
        "default config check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let bad = install.join("bad.toml");
    fs::write(
        &bad,
        config_with("[[events.\"usb.inserted\"]]\naction = \"nope\"\n"),
    )?;
    let bad_check = run_app(
        &app,
        &["--config", bad.to_str().context("path")?, "--check-config"],
    )?;
    ensure!(
        !bad_check.status.success(),
        "invalid config passed --check-config"
    );
    let missing = run_app(&app, &["--config", "missing.toml", "--once"])?;
    ensure!(!missing.status.success(), "missing config did not fail");

    let started = "[[events.\"service.started\"]]\naction = \"log.write\"\nmessage = \"hello\"\n";
    let dated = install.join("dated.toml");
    fs::write(
        &dated,
        config_with(started).replace(
            "log_file = \"test.log.txt\"\n",
            "log_file = \"dated-{date}.log\"\nlog_time = \"utc\"\n",
        ),
    )?;
    let dated_run = run_app(
        &app,
        &["--config", dated.to_str().context("path")?, "--once"],
    )?;
    let today = &hail::utc_timestamp(std::time::SystemTime::now())[..10];
    let dated_log = install.join(format!("dated-{today}.log"));
    ensure!(
        dated_run.status.success()
            && String::from_utf8(dated_run.stdout)?.contains("new file per period, utc time")
            && fs::read_to_string(&dated_log).is_ok_and(|text| text.contains("hello")),
        "dated log file not written: {}",
        dated_log.display()
    );
    // Programs hail started do not outlive it: hail.quit and --once both stop them.
    let fake = install.join(format!(
        "{}{}",
        calls::FAKE_PROCESS,
        std::env::consts::EXE_SUFFIX
    ));
    fs::copy(std::env::current_exe()?, &fake)?;
    for (name, quit, reason) in [("quitting", true, "hail.quit"), ("once", false, "--once")] {
        let config = install.join(format!("{name}.toml"));
        let quit = if quit {
            "[[events.\"service.started\"]]\naction = \"hail.quit\"\n"
        } else {
            ""
        };
        fs::write(
            &config,
            config_with(&format!(
                "[[events.\"service.started\"]]\naction = \"process.run\"\nname = \"lingering\"\ncommand = '{}'\nargs = [\"sleep\"]\n{quit}",
                fake.display()
            ))
            .replace(
                "log_file = \"test.log.txt\"\n",
                &format!("log_file = \"{name}.log\"\nstop_timeout_ms = 500\n"),
            ),
        )?;
        let mut arguments = vec!["--config", config.to_str().context("path")?];
        if quit.is_empty() {
            arguments.push("--once");
        }
        let run = run_app(&app, &arguments)?;
        let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
        let text = fs::read_to_string(install.join(format!("{name}.log")))?;
        let stopped = text
            .lines()
            .find(|line| line.contains("[process] stopped lingering (pid "))
            .unwrap_or_default();
        ensure!(
            run.status.success()
                && stdout.contains("hail: stopped 1 program(s) started by hail")
                && stopped.contains(&format!("(hail exiting: {reason}): "))
                && (stopped.contains("; exited after ") || stopped.contains(", killed (")),
            "{name}: the started program must be stopped when hail exits:\n{stdout}\n{text}"
        );
    }

    let quiet = install.join("quiet.toml");
    fs::write(
        &quiet,
        config_with(started).replace(
            "log_file = \"test.log.txt\"\n",
            "logging = false\nlog_file = \"quiet.log\"\n",
        ),
    )?;
    let quiet_check = run_app(
        &app,
        &[
            "--config",
            quiet.to_str().context("path")?,
            "--check-config",
        ],
    )?;
    let quiet_run = run_app(
        &app,
        &["--config", quiet.to_str().context("path")?, "--once"],
    )?;
    ensure!(
        String::from_utf8(quiet_check.stdout)?.contains("log disabled")
            && quiet_run.status.success()
            && String::from_utf8(quiet_run.stdout)?.contains("hail: log disabled")
            && !install.join("quiet.log").exists(),
        "logging = false still wrote a log"
    );

    let once = run_app(&app, &["--once"])?;
    ensure!(
        once.status.success(),
        "native USB polling failed: {}",
        String::from_utf8_lossy(&once.stderr)
    );
    let log = fs::read_to_string(install.join("hail.log.txt"))
        .context("service log was not written beside the executable")?;
    ensure!(
        log.contains("started (config: ") && log.contains("mode: os-events, emulator: snow)"),
        "service.started chain did not run"
    );

    for (mode, file) in [("os-events", "events.toml"), ("interval", "interval.toml")] {
        let text = if mode == "interval" {
            config_with(
                "[[events.\"service.started\"]]\naction = \"log.write\"\nmessage = \"up {mode}\"\n",
            )
        } else {
            fs::read_to_string(&context.default_config)?
        };
        let path = install.join(file);
        fs::write(&path, text.replace("hail.log.txt", "test.log.txt"))?;
        let log_path = install.join("test.log.txt");
        let _ = fs::remove_file(&log_path);
        let mut child = Command::new(&app)
            .args(["--config", path.to_str().context("path")?])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting hail in {mode} mode"))?;
        std::thread::sleep(Duration::from_millis(1500));
        let still_running = child.try_wait()?.is_none();
        let _ = child.kill();
        let output = child.wait_with_output()?;
        ensure!(
            still_running,
            "{mode} service exited instead of running until killed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(&log_path)
            .with_context(|| format!("{mode} service wrote no log"))?;
        ensure!(
            log.contains(mode) && !log.contains("polling failed"),
            "{mode} service log unexpected:\n{log}"
        );
    }
    for (action, expected) in [("hail.quit", 0), ("hail.restart", EXIT_RESTART)] {
        let path = install.join("exit.toml");
        fs::write(
            &path,
            config_with(&format!(
                "[[events.\"service.started\"]]\naction = \"log.write\"\nmessage = \"bye\"\n[[events.\"service.started\"]]\naction = \"{action}\"\n"
            )),
        )?;
        let mut child = Command::new(&app)
            .args(["--config", path.to_str().context("path")?])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                anyhow::bail!("{action} did not stop the service");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        ensure!(
            status.code() == Some(expected),
            "{action} exited with {status}, expected {expected}"
        );
    }
    for arguments in [
        &["--once", "--install"][..],
        &["--uninstall", "--config", "x.toml"][..],
        &["--check-config", "--once"][..],
        &["--watch-config", "--once"][..],
        &["--check-config", "--watch-config"][..],
        &["--uninstall", "--watch-config"][..],
    ] {
        let output = run_app(&app, arguments)?;
        ensure!(
            output.status.code() == Some(1) && !output.stderr.is_empty(),
            "{arguments:?} should be rejected"
        );
    }
    let service = run_app(&app, &["--service"])?;
    let message = String::from_utf8_lossy(&service.stderr);
    ensure!(
        service.status.code() == Some(1)
            && if cfg!(windows) {
                message.contains("only for the Windows service manager")
            } else {
                message.contains("Windows-only")
            },
        "--service outside the service manager: {message}"
    );
    ensure!(
        String::from_utf8(run_app(&app, &["--help"])?.stdout)?.contains("--install"),
        "--help lacks --install"
    );

    if !cfg!(windows) {
        let watch = run_app(&app, &["--watch-config"])?;
        ensure!(
            watch.status.code() == Some(1)
                && String::from_utf8_lossy(&watch.stderr).contains("Windows-only"),
            "--watch-config must be rejected on Linux"
        );
    }

    if cfg!(windows) {
        // Without --watch-config a changed file is only read on the next start.
        let path = install.join("unwatched.toml");
        let base = config_with(
            "[[events.\"service.started\"]]\naction = \"log.write\"\nmessage = \"unwatched up\"\n",
        )
        .replace("test.log.txt", "unwatched-one.log");
        fs::write(&path, &base)?;
        let mut child = Command::new(&app)
            .args(["--config", path.to_str().context("path")?])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::sleep(Duration::from_millis(800));
        fs::write(
            &path,
            base.replace("unwatched-one.log", "unwatched-two.log"),
        )?;
        std::thread::sleep(Duration::from_millis(3000));
        let still_running = child.try_wait()?.is_none();
        let _ = child.kill();
        let _ = child.wait();
        let one = fs::read_to_string(install.join("unwatched-one.log")).unwrap_or_default();
        ensure!(
            still_running
                && one.contains("unwatched up")
                && !one.contains("[config]")
                && !install.join("unwatched-two.log").exists(),
            "without --watch-config the file must not be reloaded (running={still_running}):\n{one}"
        );

        // Live reload: an invalid edit is rejected, a valid one switches the log file.
        let path = install.join("live.toml");
        let base = config_with("").replace("test.log.txt", "live-one.log");
        fs::write(&path, &base)?;
        let mut child = Command::new(&app)
            .args(["--config", path.to_str().context("path")?, "--watch-config"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::sleep(Duration::from_millis(800));
        fs::write(&path, base.replace("interval_ms = 250", "interval_ms = 5"))?;
        std::thread::sleep(Duration::from_millis(3000));
        fs::write(&path, base.replace("live-one.log", "live-two.log"))?;
        std::thread::sleep(Duration::from_millis(3000));
        let still_running = child.try_wait()?.is_none();
        let _ = child.kill();
        let _ = child.wait();
        let one = fs::read_to_string(install.join("live-one.log")).unwrap_or_default();
        let two = fs::read_to_string(install.join("live-two.log")).unwrap_or_default();
        ensure!(
            still_running
                && one.contains("[config] rejected changes to ")
                && one.contains("interval_ms")
                && two.contains("[config] reloaded "),
            "live reload failed (running={still_running}):\none:\n{one}\ntwo:\n{two}"
        );
    }
    println!("PASS executable CLI, config checks, --once, run-until-killed in os-events and interval modes, hail.quit/hail.restart exit codes, --service/--install/--watch-config argument handling, no reload without --watch-config and live reload with it (Windows)");
    Ok(())
}

struct ScriptedSource {
    script: VecDeque<bool>,
    timeouts: Vec<Option<Duration>>,
}

impl ChangeSource for ScriptedSource {
    fn wait(&mut self, timeout: Option<Duration>) -> Result<bool> {
        self.timeouts.push(timeout);
        match self.script.pop_front() {
            Some(true) => Ok(true),
            Some(false) => {
                std::thread::sleep(timeout.context("idle wait would block forever")?);
                Ok(false)
            }
            None => anyhow::bail!("script finished"),
        }
    }
}

fn os_events(context: &TestContext) -> Result<()> {
    let settle = Duration::from_millis(100);
    let start = Instant::now();
    let mut schedule = RescanSchedule::new(settle);
    ensure!(
        schedule.timeout(start).is_none(),
        "idle schedule must wait indefinitely"
    );
    ensure!(
        !schedule.take_due(start + settle * 10),
        "idle schedule rescanned"
    );
    schedule.signal(start);
    schedule.signal(start + settle / 2);
    ensure!(
        schedule.timeout(start + settle / 2) == Some(settle / 2),
        "repeated signals must not postpone the pending rescan"
    );
    ensure!(
        !schedule.take_due(start + settle / 2),
        "rescan ran before settling"
    );
    let mut now = start + settle;
    let mut rescans = 0;
    while schedule.timeout(now).is_some() {
        ensure!(schedule.take_due(now), "rescan was not due when expected");
        rescans += 1;
        now += settle;
    }
    ensure!(
        rescans == RescanSchedule::RESCANS,
        "expected {} settle rescans, got {rescans}",
        RescanSchedule::RESCANS
    );

    ensure!(
        is_block_uevent(b"add@/devices/pci0/usb1/1-1/host0/block/sda\0ACTION=add\0SUBSYSTEM=block\0DEVNAME=sda\0"),
        "kernel block uevent not recognised"
    );
    ensure!(
        !is_block_uevent(b"add@/devices/pci0/usb1/1-1\0ACTION=add\0SUBSYSTEM=usb\0"),
        "non-block uevent accepted"
    );
    ensure!(
        !is_block_uevent(b"libudev\0SUBSYSTEM=block\0") && !is_block_uevent(b""),
        "malformed uevent accepted"
    );

    let directory = context.root.join("os-events");
    fs::create_dir(&directory)?;
    let config = Config::parse(
        &config_with(
            r#"
[[events."usb.inserted"]]
action = "log.write"
message = "in {label}"
[[events."usb.removed"]]
action = "log.write"
message = "out {label}"
[[events."polling.failed"]]
action = "log.write"
message = "failed: {error}"
"#,
        )
        .replace(
            "mode = \"interval\"\ninterval_ms = 250\n",
            "mode = \"os-events\"\nsettle_ms = 50\n",
        ),
    )?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    let mut service = Service::new(config, log);
    let volume = disk(&context.root, "evt");
    let mut snapshots = VecDeque::from([
        vec![],
        vec![volume.clone()],
        vec![volume.clone()],
        vec![],
        vec![],
        vec![],
    ]);
    let mut scans = 0;
    // idle, signal, then idle waits that let 3 settle rescans run, then a second signal.
    let mut source = ScriptedSource {
        script: VecDeque::from([true, false, false, false, true, false, false, false]),
        timeouts: Vec::new(),
    };
    let mut reported = Vec::new();
    let result = service.run_os_events(
        &mut source,
        || {
            scans += 1;
            snapshots.pop_front().context("unexpected extra scan")
        },
        |events| reported.extend(events.iter().map(|event| event.kind)),
    );
    let error = format!(
        "{:#}",
        result
            .err()
            .context("run_os_events must only return on error")?
    );
    ensure!(
        error.contains("script finished"),
        "unexpected loop error: {error}"
    );
    ensure!(
        source.timeouts.first() == Some(&None),
        "event loop must block without a timeout while idle"
    );
    ensure!(
        scans == 2 * RescanSchedule::RESCANS as usize,
        "expected rescans driven only by signals, got {scans}"
    );
    ensure!(
        reported
            == vec![
                EventKind::UsbInserted,
                EventKind::UsbRemoved,
                EventKind::PollingFailed
            ],
        "os-events loop raised {reported:?}"
    );
    let path = service.log.path.clone();
    drop(service);
    let contents = fs::read_to_string(&path)?;
    ensure!(
        contents.starts_with(
            "in evt\nout evt\nfailed: waiting for OS hardware events: script finished"
        ),
        "os-events chain log wrong:\n{contents}"
    );

    let mut native = OsEventSource::new().context("subscribing to native OS hardware events")?;
    native.wait(Some(Duration::from_millis(200)))?;
    drop(native);
    println!(
        "PASS os-events: settle schedule, uevent filter, event-driven loop, native OS subscription"
    );
    Ok(())
}

fn run() -> Result<()> {
    let variable = |name: &str| {
        std::env::var(name)
            .map(PathBuf::from)
            .with_context(|| format!("{name} is required"))
    };
    let context = TestContext {
        root: variable("HAIL_TEST_ROOT")?,
        app: variable("HAIL_APP")?,
        default_config: variable("HAIL_DEFAULT_CONFIG")?,
    };
    fs::create_dir_all(&context.root)?;
    snapshots(&context)?;
    configuration(&context)?;
    action_chains(&context)?;
    log_location(&context)?;
    os_events(&context)?;
    disk_media(&context)?;
    exit_actions(&context)?;
    emulator_actions(&context)?;
    emulators_and_reload(&context)?;
    savefiles_and_sessions(&context)?;
    custom_emulators(&context)?;
    calls::process_tracking(&context)?;
    calls::api_and_rpc(&context)?;
    executable(&context)?;
    println!("All standalone tests passed.");
    Ok(())
}

/// File stem under which this binary acts as a fake `[emulators]` executable.
const FAKE_EMULATOR: &str = "fake-emulator";
/// Written beside the fake emulator: its working directory and arguments.
const FAKE_EMULATOR_RECORD: &str = "launched.txt";

/// Records how the fake emulator was started, then exits.
fn fake_emulator(executable: &Path) -> Result<()> {
    let directory = executable.parent().context("fake emulator has no folder")?;
    let mut record = format!("cwd={}\n", std::env::current_dir()?.display());
    for argument in std::env::args_os().skip(1) {
        record.push_str(&format!("arg={}\n", argument.to_string_lossy()));
    }
    for name in ["HAIL_IMAGE", "HAIL_SAVEFILE"] {
        let value = std::env::var(name).unwrap_or_else(|_| "<unset>".to_string());
        record.push_str(&format!("{name}={value}\n"));
    }
    let partial = directory.join("launched.partial");
    fs::write(&partial, record)?;
    fs::rename(partial, directory.join(FAKE_EMULATOR_RECORD))?;
    Ok(())
}

fn main() {
    if let Ok(executable) = std::env::current_exe() {
        if executable
            .file_stem()
            .is_some_and(|stem| stem == FAKE_EMULATOR)
        {
            std::process::exit(i32::from(fake_emulator(&executable).is_err()));
        }
        if executable
            .file_stem()
            .is_some_and(|stem| stem == calls::FAKE_PROCESS)
        {
            std::process::exit(calls::fake_process());
        }
    }
    if let Err(error) = run() {
        eprintln!("FAIL: {error:#}");
        std::process::exit(1);
    }
}
