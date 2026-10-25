#!/usr/bin/env bash
# Hardware verification on the real machine (AMD GPU with VA-API). Runs the app's
# own pipeline (`encoder --encode`) for each codec and size mode, then checks
# HDR10 metadata with ffprobe on input and output.
#
#   scripts/hw-verify.sh [--flatpak] [SDR_CLIP] [HDR10_CLIP]
#
# Without clips it generates short test clips. Results go to /tmp/encoder-hw.*/
# --flatpak tests the installed Flatpak instead of a native build (results then
# go to ~/.cache/encoder-hw.*/, since the sandbox can't see the host's /tmp).
set -uo pipefail
FLATPAK=0
if [ "${1:-}" = "--flatpak" ]; then FLATPAK=1; shift; fi
# Resolve clip paths against the caller's directory before changing directory.
args=()
for f in "$@"; do
    [ -f "$f" ] || { echo "error: $f: no such file (looked in $PWD)" >&2; exit 1; }
    args+=("$(realpath "$f")")
    if [ "$FLATPAK" = 1 ] && [[ "${args[-1]}" == /tmp/* ]]; then
        echo "error: $f is in /tmp, which the Flatpak can't see; move it to your home folder" >&2; exit 1
    fi
done
set -- "${args[@]}"
cd "$(dirname "$0")/.."
if [ "$FLATPAK" = 1 ]; then
    flatpak info --user no.oyzmo.EncodeR >/dev/null 2>&1 || { echo "error: the Flatpak isn't installed (scripts/install.sh --install-flatpak)" >&2; exit 1; }
    mkdir -p "$HOME/.cache"
    OUT=$(mktemp -d "$HOME/.cache/encoder-hw.XXXX")
    # Flatpak overrides XDG_CONFIG_HOME, so point the app at the test config directly.
    BIN=(flatpak run --env=ENCODER_CONFIG_DIR="$OUT/config/encoder" no.oyzmo.EncodeR)
else
    cargo build --release -q || exit 1
    OUT=$(mktemp -d /tmp/encoder-hw.XXXX)
    BIN=("$PWD/target/release/encoder")
fi
LOG="$OUT/report.txt"
export XDG_CONFIG_HOME="$OUT/config"
mkdir -p "$XDG_CONFIG_HOME/encoder"

SDR="${1:-}"
HDR="${2:-}"
if [ -n "$SDR" ]; then
    w=$(ffprobe -v error -select_streams v:0 -show_entries stream=width -of csv=p=0 "$SDR")
    [ "${w:-0}" -lt 640 ] && echo "warning: $SDR is only ${w}px wide; HEVC/AV1 hardware encoders need ≥ 384/320, so expect those jobs to be refused." >&2
fi
if [ -n "$HDR" ]; then
    trc=$(ffprobe -v error -select_streams v:0 -show_entries stream=color_transfer -of csv=p=0 "$HDR")
    if [ "$trc" != "smpte2084" ]; then
        echo "warning: $HDR is not HDR10 (transfer: ${trc:-unknown}); generating a test clip instead." >&2
        HDR=""
    fi
fi
if [ -z "$SDR" ]; then
    SDR="$OUT/sdr-4k.mkv"
    echo "Generating $SDR (60 s, 4K H.264, 5.1 AC-3)…"
    ffmpeg -hide_banner -v error -y -f lavfi -i "testsrc2=size=3840x2160:rate=24,noise=alls=12:allf=t" \
        -f lavfi -i "sine=f=440:sample_rate=48000" -t 60 -map 0:v -map 1:a \
        -c:v libx264 -preset veryfast -crf 18 -pix_fmt yuv420p -c:a ac3 -ac 6 -b:a 448k \
        -metadata:s:a:0 language=eng "$SDR" || exit 1
fi
if [ -z "$HDR" ]; then
    HDR="$OUT/hdr10-4k.mkv"
    echo "Generating $HDR (20 s, 4K HEVC Main10 HDR10)…"
    ffmpeg -hide_banner -v error -y -f lavfi -i "testsrc2=size=3840x2160:rate=24" -t 20 -map 0:v \
        -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le \
        -x265-params "log-level=error:hdr10=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
        "$HDR" || exit 1
fi

# Test presets: target sizes scaled to short clips (≈ 8 Mb/s video).
dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$SDR")
gb=$(awk -v d="$dur" 'BEGIN { printf "%.3f", d * 8.5e6 / 8 / 1e9 }')
cat > "$XDG_CONFIG_HOME/encoder/config.json" <<JSON
{ "preferred_languages": ["eng", "original"],
  "user_presets": [
    { "name": "T hevc target", "settings": { "codec": "Hevc", "size_mode": "TargetSize", "target_gb": $gb, "ten_bit": true } },
    { "name": "T hevc cq",     "settings": { "codec": "Hevc", "size_mode": "ConstantQuality", "qp": 25, "ten_bit": true } },
    { "name": "T h264 1080p",  "settings": { "codec": "H264", "size_mode": "ConstantQuality", "qp": 23, "ten_bit": false, "downscale": "P1080" } },
    { "name": "T av1 cq",      "settings": { "codec": "Av1",  "size_mode": "ConstantQuality", "qp": 28, "ten_bit": true } },
    { "name": "T av1 target",  "settings": { "codec": "Av1",  "size_mode": "TargetSize", "target_gb": $gb, "ten_bit": true } }
  ] }
JSON

run() { # clip preset
    local name="$2"
    echo "=== $name · $(basename "$1")" | tee -a "$LOG"
    mkdir -p "$OUT/${name// /_}"
    "${BIN[@]}" --encode "$1" --preset "$name" --out "$OUT/${name// /_}" 2>&1 | tee -a "$LOG"
    echo "exit ${PIPESTATUS[0]}" | tee -a "$LOG"
}

hdrcheck() { # file
    echo "--- ffprobe HDR side data: $1" | tee -a "$LOG"
    ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,profile,pix_fmt,color_transfer,color_primaries,color_space \
        -of default=nw=1 "$1" | tee -a "$LOG"
    ffprobe -v error -select_streams v:0 -show_frames -read_intervals %+#1 -show_entries frame=side_data_list \
        -of default=nw=1 "$1" | grep -E 'side_data_type|max_content|max_average|max_luminance|min_luminance' | tee -a "$LOG"
}

{ "${BIN[@]}" --version; [ "$FLATPAK" = 1 ] && echo "(Flatpak)"; ffmpeg -hide_banner -version | head -1; uname -r; } | tee -a "$LOG"
for p in "T hevc target" "T hevc cq" "T h264 1080p" "T av1 cq" "T av1 target"; do run "$SDR" "$p"; done
hdrcheck "$HDR"
for p in "T hevc cq" "T av1 cq"; do
    run "$HDR" "$p"
    f=$(ls "$OUT/${p// /_}"/hdr10* 2>/dev/null | head -1)
    [ -n "$f" ] && hdrcheck "$f"
done
# Interlacing: the app must detect each case from the picture and produce
# clean progressive output (deinterlace on the GPU when possible).
scancheck() { # file
    echo "--- scan check: $1" | tee -a "$LOG"
    ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,r_frame_rate,sample_aspect_ratio,display_aspect_ratio,field_order \
        -of default=nw=1 "$1" | tee -a "$LOG"
    ffmpeg -hide_banner -nostdin -i "$1" -map 0:v:0 -vf idet -f null - 2>&1 | grep -E 'Repeated|Multi' | tail -2 | sed 's/.*\] //' | tee -a "$LOG"
}
SD="testsrc2=size=1920x1080:rate=RATE,scale=720:HEIGHT:flags=bicubic"
echo "Generating interlaced test clips…"
ffmpeg -hide_banner -v error -y -f lavfi -i "testsrc2=size=1920x1080:rate=50,tinterlace=mode=interleave_top,setfield=tff" -t 20 \
    -c:v libx264 -preset veryfast -crf 18 -flags +ilme+ildct -x264opts tff=1 "$OUT/tv-1080i25.mkv"
src=${SD/RATE/50}
ffmpeg -hide_banner -v error -y -f lavfi -i "${src/HEIGHT/576},tinterlace=mode=interleave_top,setfield=tff" -t 20 \
    -c:v mpeg2video -b:v 8M -flags +ilme+ildct -top 1 -aspect 16:9 "$OUT/dvd-576i25.mkv"
src=${SD/RATE/24000\/1001}
ffmpeg -hide_banner -v error -y -f lavfi -i "${src/HEIGHT/480},telecine=first_field=top:pattern=23" -t 20 \
    -c:v libx264 -preset veryfast -crf 16 -flags +ilme+ildct -x264opts tff=1 "$OUT/film-480i30.mkv"
for clip in tv-1080i25 dvd-576i25 film-480i30; do
    run "$OUT/$clip.mkv" "T hevc cq"
    f="$OUT/T_hevc_cq/$clip-reencoded.mkv"
    [ -f "$f" ] && scancheck "$f"
done
echo "Expected: tv-1080i25 → 50/1 progressive; dvd-576i25 → 50/1, SAR 64:45 (DAR 16:9); film-480i30 → 24000/1001 progressive, no repeated fields." | tee -a "$LOG"

# Experiment: is any GPU deinterlace mode clean on this driver? (The app uses
# CPU bwdif because deinterlace_vaapi mode=default left combing in testing.)
echo "--- GPU deinterlace modes (lower TFF/BFF = cleaner; CPU bwdif for reference)" | tee -a "$LOG"
for mode in bob motion_adaptive motion_compensated default cpu-bwdif; do
    out="$OUT/deint-$mode.mkv"
    if [ "$mode" = cpu-bwdif ]; then
        ffmpeg -hide_banner -v error -y -vaapi_device /dev/dri/renderD128 -i "$OUT/tv-1080i25.mkv" \
            -vf "bwdif=mode=send_field:parity=tff:deint=all,format=nv12,hwupload" -c:v hevc_vaapi -rc_mode CQP -qp 20 "$out"
    else
        ffmpeg -hide_banner -v error -y -hwaccel vaapi -hwaccel_output_format vaapi -vaapi_device /dev/dri/renderD128 \
            -i "$OUT/tv-1080i25.mkv" -vf "deinterlace_vaapi=mode=$mode:rate=field:auto=0" -c:v hevc_vaapi -rc_mode CQP -qp 20 "$out" 2>/dev/null \
            || { echo "$mode: not supported" | tee -a "$LOG"; continue; }
    fi
    echo "$mode: $(ffmpeg -hide_banner -nostdin -i "$out" -vf idet -f null - 2>&1 | grep Multi | tail -1 | sed 's/.*\] //')" | tee -a "$LOG"
done

if command -v dovi_tool >/dev/null && [ -n "${DV_CLIP:-}" ]; then
    run "$DV_CLIP" "T hevc cq"
fi
echo
echo "Full report: $LOG"
