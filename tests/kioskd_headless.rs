//! End to end on a THROWAWAY headless compositor (skipped unless `scroll` (or `sway`) and `grim` are installed): `clck kioskd`
//! shows the overlay on the kiosk output on `kiosk on` and removes it on `kiosk off`, seen in a screenshot of the output.
//! The compositor gets its own `$XDG_RUNTIME_DIR`, so it can never reach a live session's sockets.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn have(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(bin))
            .find(|f| f.is_file())
    })
}

struct Guard(Vec<Child>, PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// Non-zero bytes of a PPM screenshot's pixel data.
fn painted(path: &Path) -> usize {
    let d = std::fs::read(path).unwrap();
    // "P6\nW H\n255\n" then the pixels
    let mut nl = 0;
    let mut at = 0;
    for (i, b) in d.iter().enumerate() {
        if *b == b'\n' {
            nl += 1;
            if nl == 3 {
                at = i + 1;
                break;
            }
        }
    }
    d[at..].iter().filter(|b| **b != 0).count()
}

#[test]
fn kiosk_on_draws_the_overlay_and_off_removes_it() {
    let Some(compositor) = have("scroll").or_else(|| have("sway")) else {
        eprintln!("no scroll/sway: skipped");
        return;
    };
    if have("grim").is_none() {
        eprintln!("no grim: skipped");
        return;
    }
    let rt = std::env::temp_dir().join(format!("clck-kioskd-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&rt).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut guard = Guard(Vec::new(), rt.clone());
    let cfg = rt.join("config");
    std::fs::create_dir_all(cfg.join("clck")).unwrap();
    std::fs::write(
        cfg.join("clck/config.json"),
        format!(
            r#"{{"kiosk_outputs":["HEADLESS-1"],"control_socket_path":"{0}/clck-control.sock","live_socket_path":"{0}/clck-live.sock","socket_path":"{0}/locker.sock"}}"#,
            rt.display()
        ),
    )
    .unwrap();
    std::fs::write(
        cfg.join("compositor.conf"),
        "output HEADLESS-1 mode 640x360 pos 0 0\noutput HEADLESS-2 mode 640x360 pos 640 0\n",
    )
    .unwrap();

    let env = |c: &mut Command| {
        c.env("XDG_RUNTIME_DIR", &rt)
            .env("XDG_CONFIG_HOME", &cfg)
            .env_remove("SWAYSOCK")
            .env_remove("I3SOCK")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    };
    let mut comp = Command::new(compositor);
    comp.arg("-c").arg(cfg.join("compositor.conf"));
    comp.env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("WLR_RENDERER", "pixman")
        .env("WLR_HEADLESS_OUTPUTS", "2");
    env(&mut comp);
    guard.0.push(comp.spawn().unwrap());
    wait("the compositor's socket", || {
        rt.join("wayland-1").exists() || rt.join("wayland-0").exists()
    });
    let display = if rt.join("wayland-1").exists() {
        "wayland-1"
    } else {
        "wayland-0"
    };

    let clck = env!("CARGO_BIN_EXE_clck");
    let mut kd = Command::new(clck);
    kd.arg("kioskd");
    env(&mut kd);
    kd.env("WAYLAND_DISPLAY", display);
    guard.0.push(kd.spawn().unwrap());
    wait("the control socket", || {
        rt.join("clck-control.sock").exists()
    });

    let kiosk = |arg: &str| -> (bool, String) {
        let mut c = Command::new(clck);
        c.args(["kiosk", arg]);
        env(&mut c);
        c.env("WAYLAND_DISPLAY", display).stdout(Stdio::piped());
        let o = c.output().unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        )
    };
    let shot = |name: &str, output: &str| -> usize {
        let f = rt.join(format!("{name}.ppm"));
        let mut g = Command::new("grim");
        g.args(["-t", "ppm", "-o", output]).arg(&f);
        env(&mut g);
        g.env("WAYLAND_DISPLAY", display);
        assert!(g.status().unwrap().success(), "grim {name}");
        painted(&f)
    };

    let (ok, out) = kiosk("status");
    assert!(ok && out.contains(r#""kiosk":"off""#), "{out}");
    assert_eq!(
        shot("a1", "HEADLESS-1"),
        0,
        "nothing painted before the command"
    );
    assert_eq!(shot("a2", "HEADLESS-2"), 0);
    let (ok, out) = kiosk("on");
    assert!(
        ok && out.contains(r#""kiosk":"on""#) && out.contains(r#"["HEADLESS-1"]"#),
        "only the named output: {out}"
    );
    wait("the overlay", || shot("b1", "HEADLESS-1") > 0);
    assert_eq!(
        shot("b2", "HEADLESS-2"),
        0,
        "an output that is not a kiosk output stays untouched"
    );
    let (ok, out) = kiosk("off");
    assert!(ok && out.contains(r#""kiosk":"off""#), "{out}");
    wait("the overlay to go", || shot("c1", "HEADLESS-1") == 0);
    drop(guard);
}
