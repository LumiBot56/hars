#!/bin/sh
# Install or upgrade hars on Linux or macOS with one command:
#
#   curl -fsSL https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/install.sh | sh
#
# It checks for Docker (and offers to install it), downloads hars, starts it
# in a container named `hars` that starts again after a reboot, waits until it
# answers and prints the address to open. Run it again to upgrade: your data
# and settings stay.
#
# Settings, as environment variables (put them before `sh`):
#   HARS_DIR    where hars keeps its data (default: ~/hars)
#   HARS_PORT   the web port (default: 8123, or the next free one)
#   HARS_IMPORT a copy of a Home Assistant 2026.6 config folder to bring over
#               before hars starts (see the README)
#   HARS_YES=1  answer yes to every question (unattended installs)
#   HARS_IMAGE  the image to run (default: the published one)
#
# Everything is in a function, so a half-downloaded script does nothing.

set -eu

main() {
	IMAGE=${HARS_IMAGE:-ghcr.io/adamt94/homeassistant-rust:latest}
	DIR=${HARS_DIR:-$HOME/hars}
	IMPORT=${HARS_IMPORT:-}
	OS=$(uname -s)
	SUDO=
	case $OS in
	Linux | Darwin) ;;
	*) die "this installer is for Linux and macOS. On Windows, see the README." ;;
	esac
	case ${HARS_PORT:-1} in
	'' | *[!0-9]*) die "HARS_PORT must be a number, not '$HARS_PORT'" ;;
	esac

	if [ -n "$IMPORT" ]; then
		[ -d "$IMPORT/.storage" ] ||
			die "HARS_IMPORT must be a copy of Home Assistant's config folder (the one with .storage in it): $IMPORT"
		IMPORT=$(cd "$IMPORT" && pwd -P)
	fi

	need_docker

	say "Downloading hars"
	if ! dk pull "$IMAGE"; then
		dk image inspect "$IMAGE" >/dev/null 2>&1 ||
			die "couldn't download $IMAGE. Check your internet connection and run this again."
		say "Couldn't download a new version; using the one already here"
	fi

	mkdir -p "$DIR/config"
	DIR=$(cd "$DIR" && pwd -P)

	# An earlier install from this folder is replaced; its data stays. Never
	# take over a `hars` container that keeps its data somewhere else.
	REPLACE=
	RUN_PORT=
	if old=$(dk inspect -f '{{range .Mounts}}{{if eq .Destination "/config"}}{{.Source}}{{end}}{{end}}' hars 2>/dev/null); then
		if [ "$old" != "$DIR/config" ]; then
			die "a container named hars already exists and keeps its data in ${old:-a Docker volume}.
       To upgrade it, run this again with HARS_DIR set to the folder that holds that config folder.
       To start fresh instead, remove it first: ${SUDO:+sudo }docker rm -f hars"
		fi
		[ -z "$IMPORT" ] ||
			die "hars is already installed in $DIR, and HARS_IMPORT only goes into a new install.
       Run the install command without HARS_IMPORT, or import by hand: see docs/INSTALL.md, \"Importing by hand\"."
		REPLACE=1
		# The port the running hars really listens on (hars.env may have been edited since).
		RUN_PORT=$(dk inspect -f '{{range .Config.Env}}{{println .}}{{end}}' hars |
			sed -n 's/^HARS_BIND=.*:\([0-9][0-9]*\)$/\1/p' | tail -n 1)
	fi

	# Check the port before stopping anything.
	ENV_FILE=$DIR/hars.env
	OLD_PORT=
	if [ -f "$ENV_FILE" ]; then
		OLD_PORT=$(sed -n 's/^HARS_BIND=.*:\([0-9][0-9]*\)$/\1/p' "$ENV_FILE")
	fi
	PORT=${HARS_PORT:-$OLD_PORT}
	if [ -z "$PORT" ]; then
		PORT=$(free_port) || exit 1
	elif [ -z "$REPLACE" ] || [ "$PORT" != "$RUN_PORT" ]; then
		# The running hars holds its own port; any other must be free.
		port_free "$PORT" ||
			die "port $PORT is in use by another program (Home Assistant?). Stop it, or pick another port with HARS_PORT."
	fi
	if [ -f "$ENV_FILE" ] && grep -q '^HARS_BIND=' "$ENV_FILE"; then
		sed "s/^HARS_BIND=.*/HARS_BIND=0.0.0.0:$PORT/" "$ENV_FILE" >"$ENV_FILE.new"
		mv "$ENV_FILE.new" "$ENV_FILE"
	elif [ -f "$ENV_FILE" ]; then
		echo "HARS_BIND=0.0.0.0:$PORT" >>"$ENV_FILE"
	else
		cat >"$ENV_FILE" <<-EOF
			# Settings for hars. Change them, then run the installer again.
			# The port hars listens on.
			HARS_BIND=0.0.0.0:$PORT
			# More detail in the logs: uncomment the next line.
			#RUST_LOG=debug
		EOF
	fi

	if [ -n "$REPLACE" ]; then
		# `docker stop` gives hars its minute to save; `rm -f` would kill it.
		say "Stopping the running hars (your data stays)"
		dk stop hars >/dev/null || die "couldn't stop the running hars; it is left as it was."
		dk rm hars >/dev/null
	fi

	# Before hars first starts, so the import doesn't meet its new files.
	if [ -n "$IMPORT" ]; then
		say "Bringing over Home Assistant's setup from $IMPORT"
		dk run --rm -v "$DIR/config:/config" -v "$IMPORT:/ha:ro" "$IMAGE" import /ha ||
			die "the import stopped; its message is above. hars isn't started. Your Home Assistant copy is unchanged."
	fi

	# Linux: share the host's network so hars finds devices on your LAN.
	# Docker Desktop (macOS) can't, so publish the port instead.
	say "Starting hars"
	if [ "$OS" = Linux ]; then
		dk run -d --name hars --restart unless-stopped --stop-timeout 60 \
			--env-file "$ENV_FILE" -v "$DIR/config:/config" \
			--network host "$IMAGE" >/dev/null
	else
		dk run -d --name hars --restart unless-stopped --stop-timeout 60 \
			--env-file "$ENV_FILE" -v "$DIR/config:/config" \
			-p "$PORT:$PORT" "$IMAGE" >/dev/null
	fi

	i=0
	until answers "$PORT"; do
		i=$((i + 1))
		if [ "$i" -ge 60 ]; then
			dk logs --tail 30 hars >&2 || true
			die "hars didn't answer on port $PORT within a minute. Its log is above."
		fi
		sleep 1
	done

	url=http://localhost:$PORT
	ip=$(lan_ip)
	d="${SUDO:+sudo }docker"
	printf '\nhars is running.\n\n'
	if [ -n "$ip" ]; then
		printf '  Open  http://%s:%s  (on this computer: %s)\n' "$ip" "$PORT" "$url"
	else
		printf '  Open  %s\n' "$url"
	fi
	if [ -n "$IMPORT" ]; then
		printf '  and log in as you did on Home Assistant.\n'
		printf '  What came across: %s/config/import-report.json\n\n' "$DIR"
	else
		printf '  and create your account.\n\n'
	fi
	printf '  Your data:  %s/config\n' "$DIR"
	printf '  Logs:       %s logs -f hars\n' "$d"
	printf '  Stop/start: %s stop hars  /  %s start hars\n' "$d" "$d"
	printf '  Upgrade:    run this installer again\n\n'
	if tty_ok; then
		if [ "$OS" = Darwin ]; then
			open "$url" 2>/dev/null || true
		elif [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v xdg-open >/dev/null 2>&1; then
			xdg-open "$url" >/dev/null 2>&1 || true
		fi
	fi
}

say() { printf '==> %s\n' "$*"; }
die() {
	printf 'Error: %s\n' "$*" >&2
	exit 1
}

# Docker, through sudo when this user isn't allowed to use it directly.
dk() {
	if [ -n "$SUDO" ]; then sudo docker "$@"; else docker "$@"; fi
}

as_root() {
	if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi
}

tty_ok() { (: </dev/tty) 2>/dev/null; }

# ask QUESTION: yes with HARS_YES=1, no when nobody can answer.
ask() {
	[ "${HARS_YES:-}" = 1 ] && return 0
	tty_ok || return 1
	printf '%s [Y/n] ' "$1" >/dev/tty
	read -r reply </dev/tty || return 1
	case $reply in '' | y | Y | yes | Yes | YES) return 0 ;; *) return 1 ;; esac
}

docker_ready() {
	command -v docker >/dev/null 2>&1 || return 1
	if docker info >/dev/null 2>&1; then
		SUDO=
		return 0
	fi
	if [ "$OS" = Linux ] && [ "$(id -u)" != 0 ] && command -v sudo >/dev/null 2>&1 &&
		docker info 2>&1 | grep -qi 'permission denied'; then
		say "Your user can't use Docker directly, so hars uses sudo (it may ask for your password)"
		if sudo docker info >/dev/null 2>&1; then
			SUDO=1
			return 0
		fi
	fi
	return 1
}

# wait_for_docker TRIES: 2 s apart.
wait_for_docker() {
	i=0
	while [ "$i" -lt "$1" ]; do
		docker_ready && return 0
		sleep 2
		i=$((i + 1))
	done
	return 1
}

need_docker() {
	# Docker Desktop's command lives here before it is first started.
	[ "$OS" = Darwin ] && PATH=$PATH:/usr/local/bin:$HOME/.docker/bin
	docker_ready && return 0

	if [ "$OS" = Darwin ]; then
		if [ ! -d /Applications/Docker.app ] && command -v brew >/dev/null 2>&1 &&
			ask "hars runs in Docker, which isn't installed. Install Docker Desktop with Homebrew now?"; then
			brew install --cask docker-desktop || brew install --cask docker ||
				die "Homebrew couldn't install Docker Desktop."
		fi
		if [ ! -d /Applications/Docker.app ]; then
			command -v docker >/dev/null 2>&1 &&
				die "Docker isn't running. Start it (Docker Desktop, OrbStack, colima...), then run this again."
			die "hars runs in Docker. Install Docker Desktop from https://www.docker.com/products/docker-desktop/ then run this again."
		fi
		say "Starting Docker Desktop (accept its terms if it asks; this can take a minute)"
		open -a Docker
		wait_for_docker 90 && return 0
		die "Docker Desktop didn't start. Open it from Applications, wait until it says it's running, then run this again."
	fi

	if ! command -v docker >/dev/null 2>&1; then
		ask "hars runs in Docker, which isn't installed. Install it now with Docker's official script?" ||
			die "hars runs in Docker. Install it (https://docs.docker.com/engine/install/ or: curl -fsSL https://get.docker.com | sudo sh), then run this again."
		say "Installing Docker"
		as_root sh -c 'curl -fsSL https://get.docker.com | sh' || die "Docker's installer failed; its output is above."
	fi
	as_root systemctl enable --now docker >/dev/null 2>&1 || as_root service docker start >/dev/null 2>&1 || true
	wait_for_docker 15 && return 0
	die "Docker is installed but not running. Start it (sudo systemctl start docker), then run this again."
}

# Whether something answers HTTP on the port.
answers() { curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$1/"; }

# Whether nothing listens on the port (curl: connection refused).
port_free() {
	rc=0
	curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$1/" || rc=$?
	[ "$rc" = 7 ]
}

# 8123, or the next free port when something (often Home Assistant) has it.
free_port() {
	p=8123
	while [ "$p" -le 8133 ]; do
		if port_free "$p"; then
			[ "$p" = 8123 ] || say "Port 8123 is in use (Home Assistant?), so hars uses $p" >&2
			echo "$p"
			return 0
		fi
		p=$((p + 1))
	done
	die "ports 8123 to 8133 are all in use; pick one with HARS_PORT"
}

lan_ip() {
	if [ "$OS" = Darwin ]; then
		ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true
	else
		hostname -I 2>/dev/null | awk '{print $1}' || true
	fi
}

main "$@"
