# Windows notes

Things learned from running Waggle builds on a real Windows 11 install
(build 26200, in Omarchy's Windows VM). Each one broke something the first
time, and each fix is cheap once known.

## Build

- **Link the C runtime statically.** A fresh Windows install has no Visual
  C++ redistributable, and a default MSVC build stops at launch with
  "VCRUNTIME140.dll was not found". `.cargo/config.toml` sets
  `+crt-static` for `x86_64-pc-windows-msvc`. Check a build with
  `llvm-readobj --coff-imports waggle.exe`: it should list only DLLs that
  ship with Windows (kernel32, user32, ws2_32 and so on), never `vcruntime*`
  or `api-ms-win-crt-*`.
- Cross-compiling from Linux with `cargo xwin` works, but it sets its own
  rustflags, so pass the static-CRT flag through
  `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static"`.
  The `LNK4099` "cannot use debug info for libcmt.lib" linker warnings that
  follow are harmless.
- On 2026-11-02, every dependency (aws-lc, the forked xmpp crate, rusqlite,
  Slint, tray-icon) built for Windows unchanged, and waggle-core's 160 unit
  tests passed on Windows.

## UI (Slint, tray-icon, toasts)

- **Use Slint's software renderer, not femtovg.** femtovg needs OpenGL 3+.
  Windows' basic display adapter, as in VMs, remote sessions and machines
  with bad drivers, fails with "Could not locate glCreateShader symbol", and
  Slint does not fall back to software by itself. The software build is
  about the same size (roughly 4.7 MB with tray and toasts) and draws fine.
- **Repaint the whole window after showing it again.** With the software
  renderer, a window hidden (close → hide to tray) and shown again only
  repaints what changed since, so static parts such as buttons stay blank
  until hovered. `Window::request_redraw()` does not help. Changing a
  property the root background depends on does: it dirties the whole window.
  Verified by screenshot in the VM.
- **Keep timers alive.** A `slint::Timer` dropped at the end of a callback
  cancels itself silently. The tray menu looked broken in the first spike
  because the timer polling `MenuEvent::receiver()` was a local inside a
  single-shot callback. Keep long-lived timers in a struct the app owns, or
  in a `thread_local!`.
- **Ask for the dark title bar when the window is created**, through
  `BackendSelector::with_winit_window_attributes_hook` (`attributes.with_theme(Dark)`,
  behind Slint's `unstable-winit-030` feature). Calling `set_theme` on the
  winit window after showing it left the title bar white.
- Create the tray icon on the main thread after Slint's event loop has
  started (in a `Timer::single_shot(0)`); winit's loop then pumps its
  messages. The tray's Open, Quit and right-click events all arrive.
- Toasts from an unpackaged app work with PowerShell's AppUserModelID for
  testing. The release needs Waggle's own AUMID
  (`SetCurrentProcessExplicitAppUserModelID`) and a Start Menu shortcut
  carrying it, created by the installer. Toast sound is still unverified: the
  VM has no audio.

## Installer

- cargo-packager's NSIS installer (built on CI, `windows-latest`) installs
  per user into `%LOCALAPPDATA%\Waggle` with no administrator rights, creates
  `Start Menu\Programs\Waggle\Waggle.lnk` and a desktop shortcut, and stamps
  the shortcut with the `identifier` as its AppUserModelID. With Waggle
  declaring the same ID at startup, toasts are credited to "Waggle" with its
  icon; a build run from a folder has no such shortcut, and its toasts show
  as "Windows PowerShell". Verified in the VM on 2026-11-03.
- The uninstaller deletes only the files it installed and removes the
  install folder only when empty, so `%LOCALAPPDATA%\Waggle\data` survives
  unless "delete app data" is ticked.
- Config keys are camelCase (`installMode`, not `installerMode`); a wrong
  key fails the packaging step, not the build.
- The `.exe` icon is embedded by `winresource` in `build.rs`, which needs
  Microsoft's resource compiler, so only Windows builds carry it.

## Testing in Omarchy's Windows VM

- `omarchy-windows-vm install`, then open the screen at http://127.0.0.1:8006.
  The host folder `/var/lib/omarchy/windows/mounts/users/1000/shared` is the
  guest's "Shared" desktop folder (`\\host.lan\Data`), and the guest can
  write back to it.
- **Never overwrite a file Windows has already opened from the share.**
  Windows keeps cached bytes, and an overwritten `.exe` then fails with "This
  app can't run on your PC" although the file on disk is fine. Use a fresh
  folder per delivery, and have the `.cmd` copy programs to `%TEMP%` before
  running them.
- Files written on the host don't appear in an open Explorer window until
  Samba drops the connection:
  `docker exec omarchy-windows smbcontrol all close-share Data`.
- Screenshot the guest without disturbing it through QEMU's monitor socket
  (`/run/shm/monitor.sock` in the container): send
  `screendump /shared/x.png -f png`. Pair that with a build that logs to a
  file in the share, and a run can be checked without anyone describing it.
- Drive the VM without touching the host: keys with the monitor's `sendkey`;
  clicks and the wheel over QEMU's VNC server (port 5900 in the container)
  with a minimal RFB client. Connect shared, and send `SetEncodings` before
  any pointer event, or QEMU treats the client as relative and the cursor
  drifts. `Win+R` then a `\\host.lan\Data\...\x.cmd` path launches a test.
