#!/usr/bin/env bash
# Build Pyo's images with BuildKit and push them, for scripts/pyo-up.sh:
#
#   scripts/build-images.sh --tag TAG --out FILE [--only a,b] [--all] [--verbose]
#   scripts/build-images.sh --dry-run [--only a,b] [--all]
#
# Writes FILE with one line per image, `<image> <tag>`: the tag each image
# is deployed with. An image whose inputs haven't changed since its last
# build keeps that build's tag (when the registry still has it), so it isn't
# built, pushed or rolled out again. The images:
#
#   pyo agent builder flows db-controller kv   deploy/images/pyo.Dockerfile
#   waf geoip                                  deploy/images/<image>.Dockerfile
#   postgres-16 postgres-17 postgres-18        deploy/images/postgres.Dockerfile
#   mysql-operator mysql-backup                deploy/images/moco/moco.Dockerfile
#
# --only a,b  builds just those images; the others keep their last tag
#             (built anyway if they have none yet)
# --all       builds every image, changed or not
# --dry-run   prints what would be built and what kept, building nothing
# --verbose   streams every build's output, each line prefixed with its image
#
# The Rust binaries are built once for all six Rust images (pyo.Dockerfile's
# `rust` stage), then hashed: a Rust image is rebuilt only when its own
# binaries (or its other inputs) changed. Up to PYO_BUILD_JOBS builds (4)
# run at once; each one's output goes to a log, shown if it fails.
#
# On a terminal it shows a board, redrawn every second: each image's state,
# time and current step (and, while Rust compiles, the crates compiled so
# far). Not on a terminal (CI, a pipe), it prints a line when a build
# starts or ends, and every 15 seconds while one runs. NO_COLOR is honored.
#
# Environment: PYO_BUILDKIT_ADDR (tcp://127.0.0.1:1234), PYO_IMAGES
# (127.0.0.1:5005/pyo, a plain-HTTP registry), PYO_CARGO_PROFILE
# (dev-release; releases build with `release`), PYO_BUILD_JOBS (4),
# PYO_BUILD_STATE (~/.cache/pyo-up/images.tsv), BUILDCTL (buildctl).
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="" OUT="" ONLY="" ALL="" DRY_RUN="" VERBOSE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG=${2:?--tag needs a value}; shift 2 ;;
    --out) OUT=${2:?--out needs a file}; shift 2 ;;
    --only) ONLY=${2:?--only needs a list of images}; shift 2 ;;
    --all) ALL=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --verbose) VERBOSE=1; shift ;;
    *) echo "build-images: unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DRY_RUN" ] || { [ -n "$TAG" ] && [ -n "$OUT" ]; } ||
  { echo "usage: build-images.sh --tag TAG --out FILE [--only a,b] [--all] [--verbose] | --dry-run [--only a,b] [--all]" >&2; exit 2; }

ADDR=${PYO_BUILDKIT_ADDR:-tcp://127.0.0.1:1234}
IMAGES=${PYO_IMAGES:-127.0.0.1:5005/pyo}
PROFILE=${PYO_CARGO_PROFILE:-dev-release}
JOBS=${PYO_BUILD_JOBS:-4}
STATE=${PYO_BUILD_STATE:-$HOME/.cache/pyo-up/images.tsv}
BUILDCTL=${BUILDCTL:-buildctl}

RUST_IMAGES=(pyo agent builder flows db-controller kv)
OTHER_IMAGES=(waf geoip postgres-16 postgres-17 postgres-18 mysql-operator mysql-backup)
ALL_IMAGES=("${RUST_IMAGES[@]}" "${OTHER_IMAGES[@]}")

declare -A WANTED=()
if [ -n "$ONLY" ]; then
  IFS=, read -ra only <<<"$ONLY"
  for i in "${only[@]}"; do
    [[ " ${ALL_IMAGES[*]} " == *" $i "* ]] || { echo "build-images: no image called '$i' (images: ${ALL_IMAGES[*]})" >&2; exit 2; }
    WANTED[$i]=1
  done
fi

# A board on a terminal (unless --verbose); plain lines otherwise.
BOARD="" BOLD="" DIM="" RED="" GREEN="" YELLOW="" CYAN="" RESET=""
if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ]; then
  [ -n "$VERBOSE" ] || BOARD=1
  if [ -z "${NO_COLOR:-}" ]; then
    BOLD=$'\033[1m' DIM=$'\033[2m' RED=$'\033[31m' GREEN=$'\033[32m'
    YELLOW=$'\033[33m' CYAN=$'\033[36m' RESET=$'\033[0m'
  fi
fi
log() { printf '%s==> %s%s\n' "$BOLD" "$*" "$RESET"; }

WORK=$(mktemp -d)
DRAWN=0 INTERRUPTED=""
cleanup() {
  local pid
  # Each job is a subshell running buildctl: stop both.
  for pid in "${!RUNNING[@]}"; do
    pkill -P "$pid" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "$WORK"
  [ -z "$BOARD" ] || printf '\033[?25h'
  [ -z "$INTERRUPTED" ] || echo "build-images: interrupted; the running builds were stopped." >&2
}
declare -A RUNNING=() STARTED=()
trap cleanup EXIT
trap 'INTERRUPTED=1; exit 130' INT TERM

# ── Inputs ─────────────────────────────────────────────────────────────────
# A hash of files' paths and contents (tracked or not; git isn't asked),
# skipping what .dockerignore keeps out of the build anyway.
hash_files() {
  find "$@" \( -name node_modules -o -name target -o -name dist -o -name .tanstack \) -prune -o \
    \( -type f -o -type l \) ! -name '*.tsbuildinfo' -print0 \
    | LC_ALL=C sort -z | xargs -0r sha256sum | sha256sum | cut -c1-64
}
hash_text() { printf '%s\n' "$@" | sha256sum | cut -c1-64; }

# What each image is built from, apart from the Rust binaries.
input_key() {
  case "$1" in
    pyo) hash_text "profile=$PROFILE" "$(hash_files deploy/images/pyo.Dockerfile package.json bun.lock dash/package.json dash/tsconfig.json dash/vite.config.ts dash/public dash/src dash/scripts/fix-shell-css.mjs)" ;;
    agent|builder|flows|db-controller|kv) hash_text "profile=$PROFILE" "$(hash_files deploy/images/pyo.Dockerfile)" ;;
    waf) hash_files deploy/images/waf.Dockerfile waf/go.mod waf/go.sum waf/cmd ;;
    geoip) hash_files deploy/images/geoip.Dockerfile ;;
    postgres-*) hash_text "$1" "$(hash_files deploy/images/postgres.Dockerfile)" ;;
    mysql-*) hash_text "$1" "$(hash_files deploy/images/moco)" ;;
  esac
}

# The binaries in each Rust image.
binaries() {
  case "$1" in
    pyo) echo "pyo-api pyo" ;;
    agent) echo pyo-agent ;;
    builder) echo pyo-builder ;;
    flows) echo pyo-flows ;;
    db-controller) echo pyo-db-controller ;;
    kv) echo pyo-kv ;;
  esac
}

# image → repository:tag in the registry (postgres-16 → postgres:16-TAG).
ref() {
  case "$1" in
    postgres-*) echo "$IMAGES/postgres:${1#postgres-}-$2" ;;
    *) echo "$IMAGES/$1:$2" ;;
  esac
}

# Whether the registry still has image at tag.
in_registry() {
  local r path tag
  r=$(ref "$1" "$2")
  path=${r#*/} tag=${path##*:} path=${path%:*}
  curl -fsS -o /dev/null -m 10 -I \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
    "http://${IMAGES%%/*}/v2/$path/manifests/$tag" 2>/dev/null
}

# ── The last builds (image key tag) ────────────────────────────────────────
declare -A LAST_KEY=() LAST_TAG=() KEY=() RESULT=()
if [ -r "$STATE" ]; then
  while read -r image key tag; do
    [ -n "${tag:-}" ] || continue
    LAST_KEY[$image]=$key LAST_TAG[$image]=$tag
  done <"$STATE"
fi

# Keep image's last tag if its inputs are unchanged (or, with --only, it
# wasn't asked for) and the registry still has it.
reusable() {
  local image=$1 last=${LAST_TAG[$1]:-}
  [ -z "$ALL" ] && [ -n "$last" ] || return 1
  if [ -n "$ONLY" ]; then
    [ -z "${WANTED[$image]:-}" ] || return 1
  else
    [ "${KEY[$image]}" = "${LAST_KEY[$image]:-}" ] || return 1
  fi
  in_registry "$image" "$last"
}

# ── Builds ─────────────────────────────────────────────────────────────────
buildctl_build() {
  "$BUILDCTL" --addr "$ADDR" build --frontend dockerfile.v0 --progress plain "$@"
}

build_cmd() {
  local image=$1 push
  push="type=image,name=$(ref "$image" "$TAG"),push=true"
  case "$image" in
    bins)
      buildctl_build --local context=. --local dockerfile=deploy/images --opt filename=pyo.Dockerfile \
        --opt target=bins --opt build-arg:CARGO_PROFILE="$PROFILE" --output "type=local,dest=$WORK/bins" ;;
    web)
      # The dashboard, ahead of the pyo image: no output, it just ends up
      # in BuildKit's cache.
      buildctl_build --local context=. --local dockerfile=deploy/images --opt filename=pyo.Dockerfile \
        --opt target=web ;;
    pyo|agent|builder|flows|db-controller|kv)
      buildctl_build --local context=. --local dockerfile=deploy/images --opt filename=pyo.Dockerfile \
        --opt target="$image" --opt build-arg:CARGO_PROFILE="$PROFILE" --output "$push" ;;
    waf|geoip)
      buildctl_build --local context=. --local dockerfile=deploy/images --opt filename="$image.Dockerfile" --output "$push" ;;
    postgres-*)
      buildctl_build --local context=deploy/images --local dockerfile=deploy/images --opt filename=postgres.Dockerfile \
        --opt build-arg:PG_MAJOR="${image#postgres-}" --output "$push" ;;
    mysql-*)
      buildctl_build --local context=deploy/images/moco --local dockerfile=deploy/images/moco --opt filename=moco.Dockerfile \
        --opt target="${image#mysql-}" --output "$push" ;;
  esac
}

# ── Progress ───────────────────────────────────────────────────────────────
# Each job's state (queued, building, reused, done, failed, stopped) and the
# line shown for it; ORDER is the board's rows.
declare -A ST=() ENDED=() DETAIL=() SEEN=() NEXT_NOTE=()
ORDER=()

label() {
  case "$1" in
    bins) echo "rust binaries" ;;
    web) echo dashboard ;;
    *) echo "$1" ;;
  esac
}

fmt_time() { printf '%d:%02d' "$(($1 / 60))" "$(($1 % 60))"; }

# What a build is doing, from its plain BuildKit log: the newest step that
# hasn't finished (`#12 [rust 5/9] RUN cargo build …` → "rust 5/9 cargo
# build"), and for cargo the crates compiled so far and the latest one.
step_of() {
  awk '
    function short(s,   n, w, i, j, out, max) {
      if (s ~ /^\[[^]]*\] /) {
        stage = substr(s, 2, index(s, "]") - 2); s = substr(s, index(s, "]") + 2)
      } else stage = ""
      n = split(s, w, " "); i = 1; max = 3; out = ""
      if (w[1] == "RUN") { i = 2; max = 2 }
      else if (w[1] ~ /^(COPY|ADD)$/) { out = w[1]; i = 2; max = 2 }
      while (i <= n && w[i] ~ /^--/) i++
      if (w[1] == "RUN" && w[i] == "set" && w[i + 1] ~ /^-[a-z]+;?$/) i += 2
      for (j = 0; i <= n && j < max; i++) {
        if (w[i] ~ /^(&&|\|\||\||;)$/) break
        out = out (out != "" ? " " : "") w[i]; j++
        if (out ~ /;$/) { sub(/;$/, "", out); break }
      }
      return (stage != "" ? stage " " : "") out
    }
    /^#[0-9]+ / {
      id = $1; rest = substr($0, length(id) + 2)
      if (rest ~ /^ERROR/) err = id
      if (rest ~ /^(DONE|CACHED|ERROR|CANCELED)/) { done[id] = 1; next }
      if (!(id in name)) { name[id] = rest; ids[++nids] = id; next }
      if (rest ~ /^[0-9.]+ +Compiling /) {
        crates++; c = rest; sub(/^[0-9.]+ +Compiling /, "", c)
        split(c, cw, " "); crate = cw[1] " " cw[2]
      } else if (rest !~ /^[0-9.]+ / && rest !~ /^sha256:/) sub_[id] = rest
    }
    END {
      cur = ""
      for (i = nids; i >= 1; i--) if (!(ids[i] in done)) { cur = ids[i]; break }
      if (cur == "" && nids > 0) cur = ids[nids]
      if (err != "") cur = err
      if (cur != "") {
        out = short(name[cur])
        if (name[cur] !~ /^\[/ && (cur in sub_)) out = out ": " sub_[cur]
      }
      if (crates > 0) out = out " · Compiling " crate " (" crates " crates)"
      print out
    }' "$WORK/$1.log" 2>/dev/null || true
}

# One board row for job, cut to the terminal's width (cols).
row() {
  local job=$1 cols=$2 st=${ST[$1]} t="" color="" detail width
  case "$st" in
    building) color=$CYAN t=$(fmt_time "$((SECONDS - STARTED[$job]))") ;;
    done) color=$GREEN t=$(fmt_time "$((ENDED[$job] - STARTED[$job]))") ;;
    failed) color=$RED t=$(fmt_time "$((ENDED[$job] - STARTED[$job]))") ;;
    stopped) color=$YELLOW t=$(fmt_time "$((ENDED[$job] - STARTED[$job]))") ;;
    *) color=$DIM ;;
  esac
  detail=${DETAIL[$job]:-}
  width=$((cols - 34))
  [ "$width" -gt 0 ] || width=0
  [ "${#detail}" -le "$width" ] || detail="${detail:0:$((width > 1 ? width - 1 : 0))}…"
  printf -v ROW '  %-14s %s%-8s%s %5s  %s' "$(label "$job")" "$color" "$st" "$RESET" "$t" "$detail"
}

draw() {
  local cols job n=0 building=0 queued=0 done_=0 reused=0 failed=0 total
  cols=$(tput cols 2>/dev/null) || cols=80
  [[ "$cols" =~ ^[0-9]+$ ]] && [ "$cols" -ge 40 ] || cols=80
  [ "$DRAWN" -eq 0 ] || printf '\033[%dA' "$DRAWN"
  for job in "${ORDER[@]}"; do
    row "$job" "$cols"
    printf '\r\033[2K%s\n' "$ROW"
    n=$((n + 1))
    case "${ST[$job]}" in
      building) building=$((building + 1)) ;;
      queued) queued=$((queued + 1)) ;;
      done) done_=$((done_ + 1)) ;;
      reused) reused=$((reused + 1)) ;;
      failed) failed=$((failed + 1)) ;;
    esac
  done
  total="  ${BOLD}total $(fmt_time "$((SECONDS - t0))")${RESET}  $building building · $queued queued · $done_ done · $reused reused"
  [ "$failed" -eq 0 ] || total+=" · ${RED}$failed failed${RESET}"
  printf '\r\033[2K%s\n' "$total"
  DRAWN=$((n + 1))
}

# --verbose: the log lines job wrote since the last call (all of them, a
# last unfinished line too, with all=1), prefixed with its name.
stream() {
  local job=$1 all=${2:-} n f=$WORK/$1.log
  [ -e "$f" ] || return 0
  if [ -n "$all" ]; then n=$(awk 'END { print NR }' "$f"); else n=$(wc -l <"$f"); fi
  if [ "$n" -gt "${SEEN[$job]:-0}" ]; then
    awk -v a="$((${SEEN[$job]:-0} + 1))" -v b="$n" -v p="$CYAN$(printf '%-14s' "$(label "$job")")$RESET| " \
      'NR >= a && NR <= b { print p $0 } NR > b { exit }' "$f"
    SEEN[$job]=$n
  fi
}

# Every second: the board, or the new log lines, or every 15 s a line per
# running build.
tick() {
  local job
  for job in "${ORDER[@]}"; do
    [ "${ST[$job]}" = building ] || continue
    if [ -n "$VERBOSE" ]; then
      stream "$job"
    elif [ -n "$BOARD" ] || [ "$SECONDS" -ge "${NEXT_NOTE[$job]}" ]; then
      DETAIL[$job]=$(step_of "$job")
    fi
    if [ -z "$BOARD" ] && [ -z "$VERBOSE" ] && [ "$SECONDS" -ge "${NEXT_NOTE[$job]}" ]; then
      printf '    %s %s  %s\n' "$(label "$job")" "$(fmt_time "$((SECONDS - STARTED[$job]))")" "${DETAIL[$job]:-starting}"
      NEXT_NOTE[$job]=$((SECONDS + 15))
    fi
  done
  [ -z "$BOARD" ] || draw
}

QUEUE=()
start() {
  local job=$1
  # The exit status goes to a file: the loop below checks for it.
  { rc=0; build_cmd "$job" >"$WORK/$job.log" 2>&1 || rc=$?; echo "$rc" >"$WORK/$job.rc~"; mv "$WORK/$job.rc~" "$WORK/$job.rc"; } &
  RUNNING[$!]=$job STARTED[$job]=$SECONDS ST[$job]=building DETAIL[$job]="" NEXT_NOTE[$job]=$((SECONDS + 15))
  [ -z "$BOARD" ] || return 0
  case "$job" in
    bins) log "Building the Rust binaries ($PROFILE)" ;;
    web) log "Building the dashboard" ;;
    *) log "Building $job:$TAG" ;;
  esac
}

# Rust images, once the binaries are hashed: rebuilt if their binaries or
# inputs changed.
queue_rust() {
  local image b sums
  for image in "${RUST_IMAGES[@]}"; do
    [ -z "${RESULT[$image]:-}" ] || continue
    sums=""
    for b in $(binaries "$image"); do
      sums+=$(sha256sum <"$WORK/bins/$b" | cut -c1-64)
    done
    KEY[$image]=$(hash_text "${KEY[$image]}" "$sums")
    if reusable "$image"; then
      RESULT[$image]=${LAST_TAG[$image]} ST[$image]=reused DETAIL[$image]="kept ${LAST_TAG[$image]}"
      [ -n "$BOARD" ] || printf '    %s unchanged, keeps %s\n' "$image" "${LAST_TAG[$image]}"
    else
      QUEUE+=("$image") DETAIL[$image]=""
    fi
  done
}

finished() {
  local job=$1 status=$2 other
  ENDED[$job]=$SECONDS
  [ -z "$VERBOSE" ] || stream "$job" all
  if [ "$status" -ne 0 ]; then
    ST[$job]=failed DETAIL[$job]=$(step_of "$job")
    for other in "${RUNNING[@]}"; do ST[$other]=stopped ENDED[$other]=$SECONDS; done
    [ -z "$BOARD" ] || draw
    echo >&2
    echo "${RED}${BOLD}build-images: building $(label "$job") failed${RESET} after $(fmt_time "$((SECONDS - STARTED[$job]))"). The end of its log:" >&2
    tail -n 40 "$WORK/$job.log" | sed "s/^/  ${RED}|${RESET} /" >&2
    exit 1
  fi
  ST[$job]="done" DETAIL[$job]=""
  [ -n "$BOARD" ] || printf '    %s done (%s)\n' "$(label "$job")" "$(fmt_time "$((SECONDS - STARTED[$job]))")"
  case "$job" in
    bins) queue_rust ;;
    web) ;;
    *) RESULT[$job]=$TAG ;;
  esac
}

for image in "${ALL_IMAGES[@]}"; do KEY[$image]=$(input_key "$image"); done
for image in "${OTHER_IMAGES[@]}"; do
  if reusable "$image"; then RESULT[$image]=${LAST_TAG[$image]}; else QUEUE+=("$image"); fi
done
# The Rust binaries first: the longest build, which the Rust images wait
# for. With --only, the Rust images not asked for keep their last tag
# (if the registry has it); when that's all of them, the binaries aren't
# built at all.
rust=1
if [ -n "$ONLY" ]; then
  rust=""
  for image in "${RUST_IMAGES[@]}"; do
    if reusable "$image"; then RESULT[$image]=${LAST_TAG[$image]}; else rust=1; fi
  done
fi

if [ -n "$DRY_RUN" ]; then
  for image in "${ALL_IMAGES[@]}"; do
    if [ -n "${RESULT[$image]:-}" ]; then
      printf '%-15s keep %s\n' "$image" "${RESULT[$image]}"
    elif [[ " ${RUST_IMAGES[*]} " == *" $image "* ]] && [ -z "$ALL" ] && [ -z "${WANTED[$image]:-}" ] \
      && [ -n "${LAST_TAG[$image]:-}" ] && in_registry "$image" "${LAST_TAG[$image]}"; then
      printf '%-15s build if its binaries or inputs changed, else keep %s\n' "$image" "${LAST_TAG[$image]}"
    else
      printf '%-15s build\n' "$image"
    fi
  done
  exit 0
fi

if [ -n "$rust" ]; then
  # The dashboard alongside, unless the pyo image is kept anyway.
  if [ -n "$ONLY" ] && [ -z "${WANTED[pyo]:-}" ] && reusable pyo; then
    QUEUE=(bins "${QUEUE[@]}")
  else
    QUEUE=(bins web "${QUEUE[@]}")
  fi
fi

# The board's rows, in order.
for job in bins web; do
  [[ " ${QUEUE[*]} " == *" $job "* ]] && ORDER+=("$job") || true
done
for image in "${ALL_IMAGES[@]}"; do
  ORDER+=("$image")
  if [ -n "${RESULT[$image]:-}" ]; then
    ST[$image]=reused DETAIL[$image]="kept ${RESULT[$image]}"
  else
    ST[$image]=queued
    [[ " ${QUEUE[*]} " == *" $image "* ]] || DETAIL[$image]="waiting for the Rust binaries"
  fi
done
for job in bins web; do [ -n "${ST[$job]:-}" ] || ST[$job]=queued; done

t0=$SECONDS
[ -z "$BOARD" ] || printf '\033[?25l'
while [ "${#QUEUE[@]}" -gt 0 ] || [ "${#RUNNING[@]}" -gt 0 ]; do
  while [ "${#QUEUE[@]}" -gt 0 ] && [ "${#RUNNING[@]}" -lt "$JOBS" ]; do
    start "${QUEUE[0]}"
    QUEUE=("${QUEUE[@]:1}")
  done
  ended=""
  for pid in "${!RUNNING[@]}"; do
    job=${RUNNING[$pid]}
    [ -e "$WORK/$job.rc" ] || continue
    wait "$pid" 2>/dev/null || true
    unset "RUNNING[$pid]"
    ended=1
    finished "$job" "$(cat "$WORK/$job.rc")"
  done
  tick
  [ -n "$ended" ] || sleep 1
done
if [ -n "$BOARD" ]; then
  draw
  printf '\033[?25h'
fi

# ── Results ────────────────────────────────────────────────────────────────
mkdir -p "$(dirname "$STATE")"
: >"$WORK/state" ; : >"$WORK/out"
built=() kept=()
for image in "${ALL_IMAGES[@]}"; do
  tag=${RESULT[$image]:?"build-images: no tag for $image"}
  printf '%s %s\n' "$image" "$tag" >>"$WORK/out"
  if [ "$tag" = "$TAG" ]; then
    built+=("$image")
    printf '%s %s %s\n' "$image" "${KEY[$image]}" "$tag" >>"$WORK/state"
  else
    kept+=("$image")
    printf '%s %s %s\n' "$image" "${LAST_KEY[$image]:-}" "$tag" >>"$WORK/state"
  fi
done
mv "$WORK/state" "$STATE"
mv "$WORK/out" "$OUT"
echo "Built (${#built[@]}, $(fmt_time "$((SECONDS - t0))")): ${built[*]:-none}"
[ "${#kept[@]}" -eq 0 ] || echo "Unchanged, kept their last tag: ${kept[*]}"
