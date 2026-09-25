#[cfg(target_os = "freebsd")]
#[test]
fn probe_sndstat() {
    let devs = maolan_engine::audio_devices::discover_freebsd_audio_devices();
    eprintln!(
        "devices: {:?}",
        devs.iter().map(|d| &d.id).collect::<Vec<_>>()
    );
    assert!(!devs.is_empty(), "no devices discovered");
}
