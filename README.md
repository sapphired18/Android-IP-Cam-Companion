# IP Cam Webcam

A native GTK4 / libadwaita app that turns an Android phone running
[Android IP Camera](https://github.com/DigitallyRefined/android-ip-camera) into a
virtual webcam on Linux. It controls the phone over its HTTP API and feeds the
video stream to a `v4l2loopback` device through `ffmpeg`, so Zoom, Meet, OBS,
browsers and GNOME Camera can use it like a regular camera.

> Unofficial companion app, not affiliated with the Android IP Camera project.

## Features

- **Virtual webcam:** phone stream → `ffmpeg` → `/dev/videoN` (v4l2loopback).
- **Codec options:** source stream H.264 or MJPEG; software decoding or hardware
  decoding (VAAPI, NVDEC/CUDA, `*_cuvid`, `*_qsv`); output pixel format
  (`yuyv422`, `yuv420p`, `nv12`, `rgb24`, `bgr24`, `gray`); optional scale and frame rate.
- **Phone controls:** camera/lens selection, resolution (read from the phone), rotate,
  mirror, zoom, exposure, contrast, autofocus or manual focus distance, torch.
- **Capture:** start/stop recording on the phone, save a full-resolution snapshot to
  your Pictures folder.
- **Reads the phone's state:** *Connect* fills the controls from `/info.json`
  (lenses, supported sizes, current lens settings, battery level).
- **GNOME Camera / PipeWire support:** optionally restarts WirePlumber after the stream
  starts so PipeWire-based apps detect the virtual camera.
- **Robust streaming:** auto-restart with back-off, and no retry loop on
  `401`/`403`/`429` so the phone doesn't lock you out.
- Passwords are masked in the log.

## Requirements

- Linux with a systemd user session (PipeWire + WirePlumber if you want the Camera app to see the device)
- `ffmpeg`
- `v4l2loopback` kernel module (`v4l2loopback-dkms`)
- `polkit` (`pkexec`) for the *Load module* button
- GTK 4 and libadwaita **1.4 or newer**
- Rust toolchain and development packages to build

On the phone: the [Android IP Camera](https://github.com/DigitallyRefined/android-ip-camera)
app with streaming running.

## Build and install

Install the build dependencies:

```sh
# Debian / Ubuntu
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libssl-dev \
                 ffmpeg v4l2loopback-dkms

# Fedora
sudo dnf install gcc pkgconf gtk4-devel libadwaita-devel openssl-devel ffmpeg
# (v4l2loopback comes from RPM Fusion / akmods)

# Arch
sudo pacman -S base-devel gtk4 libadwaita openssl ffmpeg v4l2loopback-dkms
```

Install Rust if needed (<https://rustup.rs>), then:

```sh
git clone <this repository>
cd <repository folder>
cargo install --path . --root ~/.local
```

The binary lands in `~/.local/bin/ipcam-webcam`. Make sure `~/.local/bin` is in your `PATH`.
To add it to the application menu:

```sh
install -Dm644 io.github.IpCamWebcam.desktop \
  ~/.local/share/applications/io.github.IpCamWebcam.desktop
```

To only build without installing: `cargo build --release` (binary in `target/release/`).

## Usage

1. Start streaming in the Android IP Camera app and note the phone's IP address and port
   (default `4444`).
2. Run `ipcam-webcam`.
3. **Camera tab:** enter host, port, username and password (enable HTTPS unless you turned
   TLS off on the phone), then press **Connect**.
4. **Webcam tab:** click **Load module** (asks for admin rights) to create the virtual
   camera, and pick the source codec, decoder and pixel format.
5. Press **Start webcam** in the header bar. Select **Android IP Cam** in your video app.

Tips:

- Right after *Connect*, the phone may be on a low preset resolution. Pick `high`, `max` or an
  explicit size such as `1920x1080` in the Resolution row.
- MJPEG has lower latency and CPU use but needs more bandwidth. H.264 is the opposite.
- `yuyv422` is the most widely compatible output format.

## Virtual camera module

*Load module* runs:

```sh
pkexec modprobe v4l2loopback devices=1 video_nr=<N> card_label="Android IP Cam" exclusive_caps=1
```

where `<N>` is taken from the Device field (default `/dev/video10`). If you have a built-in
webcam, avoid `video0`.

The module is gone after a reboot. To load it at boot:

```sh
# /etc/modules-load.d/v4l2loopback.conf
v4l2loopback

# /etc/modprobe.d/v4l2loopback.conf
options v4l2loopback devices=1 video_nr=10 card_label="Android IP Cam" exclusive_caps=1
```

### Unloading the module

PipeWire keeps the device open, so stop WirePlumber first:

```sh
systemctl --user stop wireplumber
sudo modprobe -r v4l2loopback
systemctl --user start wireplumber
```

If it still says "in use", find the holder with `sudo fuser -v /dev/video*`.

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| `401 Unauthorized` in the log | Wrong username or password. The app does not retry, to avoid a lockout. Re-enter them. |
| `429 Too Many Requests` | The phone rate-limited you after failed logins. Wait a minute, or toggle the server off and on in the phone app. |
| Camera app doesn't list the device | Keep the stream running and press **Restart now** in the *WirePlumber* row (or leave *Refresh PipeWire on start* on). Apps that read `/dev/video*` directly (OBS, Firefox, Cheese) don't need this. |
| `/dev/videoN not found` | Press **Load module** and make sure the Device field matches the created device. |
| `deprecated pixel format used` warning | Harmless; MJPEG uses full-range YUV and swscale only warns about it. |
| Resolution list is empty | The phone didn't return its camera list. Check host, port, credentials and HTTPS, then press **Connect** again. |
| Hardware decoder errors | Switch the Decoder to `auto (software)`, or check your VAAPI/NVDEC setup. |

## Configuration

Settings are stored in `~/.config/ipcam-webcam.json` (readable only by you). This includes
the password in plain text. Delete the file to reset the app.

## Notes and limitations

- Video only. Phone audio (`/audio`) is not routed to a virtual microphone.
- v4l2 devices accept raw video only, so the "codec" choice applies to the incoming stream
  and decoder, not to the virtual camera's output.
- TLS certificate verification is disabled for the phone connection, because the Android app
  generates a self-signed certificate by default. Use it on a network you trust.
- Control requests use HTTP Basic authentication. The `ffmpeg` stream puts the credentials
  in the URL, as the Android app's own README does.

## Acknowledgements

- [DigitallyRefined/android-ip-camera](https://github.com/DigitallyRefined/android-ip-camera)
  for the phone app and its HTTP API.
- [v4l2loopback](https://github.com/umlaeute/v4l2loopback), [FFmpeg](https://ffmpeg.org),
  [GTK](https://gtk.org) and [libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/),
  [gtk4-rs](https://gtk-rs.org).

## License
GNU General Public License v3.0

