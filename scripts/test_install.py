"""install.sh starts hars the right way for each system, and upgrades safely.

Fake `docker`, `curl` and `uname` on PATH record what the installer does, so
nothing is downloaded or run. `curl` answers on a port once the fake
`docker run` has "started" hars, and FAKE_BUSY lists ports already in use.
"""
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent.parent / "install.sh"

# The only real commands the installer sees: a real docker (CI runners have
# one) or sudo must never be found.
TOOLS = ["sh", "mkdir", "sed", "grep", "mv", "cat", "id", "sleep", "awk",
         "touch", "rm", "tail"]

DOCKER = r"""#!/bin/sh
printf '%s\n' "$*" >> "$FAKE/calls"
case $1 in
info) exit 0 ;;
inspect)
    [ -n "${FAKE_MOUNT+x}" ] || exit 1
    case $* in
    *Mounts*) printf '%s\n' "$FAKE_MOUNT" ;;
    *Env*) [ -n "${FAKE_STOPPED:-}" ] || cat "$FAKE/container_env" 2>/dev/null ;;
    esac
    exit 0 ;;
run)
    touch "$FAKE/running"
    # The container keeps the env file's values as they were at `run`.
    prev=
    for a; do
        [ "$prev" = --env-file ] && grep -v '^#' "$a" > "$FAKE/container_env"
        prev=$a
    done ;;
esac
exit 0
"""

# Connection refused (7) on a free port, an answer once hars runs.
CURL = r"""#!/bin/sh
for a; do url=$a; done
port=${url#http://127.0.0.1:}; port=${port%/}
case " $FAKE_BUSY " in *" $port "*) exit 0 ;; esac
[ -e "$FAKE/running" ] && exit 0
exit 7
"""


class InstallTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = pathlib.Path(tmp.name)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for tool in TOOLS:
            (self.bin / tool).symlink_to(shutil.which(tool))
        self.home = self.tmp / "home"
        self.home.mkdir()

    def fake(self, name, body):
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def install(self, system="Linux", docker=True, **env):
        self.fake("uname", f"#!/bin/sh\necho {system}\n")
        self.fake("curl", CURL)
        if docker:
            self.fake("docker", DOCKER)
        (self.tmp / "running").unlink(missing_ok=True)
        full_env = {
            "PATH": str(self.bin),
            "HOME": str(self.home),
            "FAKE": str(self.tmp),
            "FAKE_BUSY": "",
            **env,
        }
        # No terminal: nobody can answer a question, and no browser opens.
        result = subprocess.run([str(self.bin / "sh"), str(SCRIPT)], env=full_env,
                                stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, check=False, start_new_session=True)
        calls = self.tmp / "calls"
        lines = calls.read_text().splitlines() if calls.exists() else []
        calls.unlink(missing_ok=True)
        return result, lines

    def data(self):
        return (self.home / "hars").resolve()

    def run_call(self, calls):
        runs = [c for c in calls if c.startswith("run -d ")]
        self.assertEqual(len(runs), 1, calls)
        return runs[0]

    def test_linux_uses_the_host_network_and_keeps_data_in_home(self):
        result, calls = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        run = self.run_call(calls)
        self.assertIn("--name hars --restart unless-stopped", run)
        self.assertIn("--network host", run)
        self.assertIn(f"-v {self.data()}/config:/config", run)
        self.assertIn(f"--env-file {self.data()}/hars.env", run)
        self.assertTrue(run.endswith("ghcr.io/adamt94/homeassistant-rust:latest"))
        self.assertIn("HARS_BIND=0.0.0.0:8123\n",
                      (self.data() / "hars.env").read_text())
        self.assertIn("http://localhost:8123", result.stdout)
        self.assertIn("docker logs -f hars", result.stdout)

    def test_macos_publishes_the_port(self):
        result, calls = self.install(system="Darwin")
        self.assertEqual(result.returncode, 0, result.stderr)
        run = self.run_call(calls)
        self.assertIn("-p 8123:8123", run)
        self.assertNotIn("--network host", run)

    def test_a_busy_port_moves_to_the_next_free_one(self):
        result, calls = self.install(system="Darwin", FAKE_BUSY="8123 8124")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("-p 8125:8125", self.run_call(calls))
        self.assertIn("Port 8123 is in use", result.stderr)
        self.assertIn("http://localhost:8125", result.stdout)

    def test_running_again_replaces_the_container_and_keeps_settings(self):
        result, _ = self.install(FAKE_BUSY="8123")
        self.assertEqual(result.returncode, 0, result.stderr)
        env_file = self.data() / "hars.env"
        env_file.write_text(env_file.read_text().replace("#RUST_LOG", "RUST_LOG"))
        result, calls = self.install(FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 0, result.stderr)
        # Stopped first, so hars saves; then removed and started again.
        self.assertLess(calls.index("stop hars"), calls.index("rm hars"))
        self.assertLess(calls.index("rm hars"), calls.index(self.run_call(calls)))
        text = env_file.read_text()
        self.assertIn("HARS_BIND=0.0.0.0:8124\n", text)
        self.assertIn("\nRUST_LOG=debug\n", text)

    def test_hars_port_changes_the_port_of_an_install(self):
        self.install()
        result, calls = self.install(system="Darwin", HARS_PORT="9000",
                                     FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("-p 9000:9000", self.run_call(calls))
        self.assertIn("HARS_BIND=0.0.0.0:9000\n",
                      (self.data() / "hars.env").read_text())

    def test_never_takes_over_a_container_with_data_elsewhere(self):
        result, calls = self.install(FAKE_MOUNT="/srv/other/config")
        self.assertEqual(result.returncode, 1)
        self.assertIn("/srv/other/config", result.stderr)
        self.assertNotIn("stop hars", calls)
        self.assertFalse(any(c.startswith("run ") for c in calls))

    def test_imports_home_assistant_before_the_first_start(self):
        ha = self.tmp / "ha-copy"
        (ha / ".storage").mkdir(parents=True)
        result, calls = self.install(HARS_IMPORT=str(ha))
        self.assertEqual(result.returncode, 0, result.stderr)
        image = "ghcr.io/adamt94/homeassistant-rust:latest"
        imp = (f"run --rm -v {self.data()}/config:/config "
               f"-v {ha.resolve()}:/ha:ro {image} import /ha")
        self.assertLess(calls.index(imp), calls.index(self.run_call(calls)))
        self.assertIn("log in as you did on Home Assistant", result.stdout)

    def test_refuses_an_import_folder_that_is_not_a_config_folder(self):
        result, calls = self.install(HARS_IMPORT=str(self.tmp))
        self.assertEqual(result.returncode, 1)
        self.assertIn("with .storage in it", result.stderr)
        self.assertEqual(calls, [])

    def test_refuses_a_chosen_port_that_is_in_use(self):
        result, calls = self.install(HARS_PORT="8123", FAKE_BUSY="8123")
        self.assertEqual(result.returncode, 1)
        self.assertIn("port 8123 is in use", result.stderr)
        self.assertFalse(any(c.startswith("run ") for c in calls))

    def test_a_busy_new_port_leaves_the_running_hars_alone(self):
        self.install()
        result, calls = self.install(HARS_PORT="9000", FAKE_BUSY="9000",
                                     FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 1)
        self.assertIn("port 9000 is in use", result.stderr)
        self.assertNotIn("stop hars", calls)

    def test_a_port_edited_in_hars_env_is_checked_too(self):
        self.install()
        env_file = self.data() / "hars.env"
        env_file.write_text(env_file.read_text().replace(":8123", ":8130"))
        result, calls = self.install(FAKE_BUSY="8130",
                                     FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 1)
        self.assertIn("port 8130 is in use", result.stderr)
        self.assertNotIn("stop hars", calls)

    def test_a_stopped_hars_does_not_hold_its_port(self):
        self.install()
        result, calls = self.install(FAKE_BUSY="8123", FAKE_STOPPED="1",
                                     FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 1)
        self.assertIn("port 8123 is in use", result.stderr)
        self.assertNotIn("stop hars", calls)

    def test_an_import_into_an_existing_install_is_refused_first(self):
        self.install()
        ha = self.tmp / "ha-copy"
        (ha / ".storage").mkdir(parents=True)
        result, calls = self.install(HARS_IMPORT=str(ha),
                                     FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 1)
        self.assertIn("only goes into a new install", result.stderr)
        self.assertNotIn("stop hars", calls)

    def test_rejects_a_port_that_is_not_a_number(self):
        result, calls = self.install(HARS_PORT="80a",
                                     FAKE_MOUNT=f"{self.data()}/config")
        self.assertEqual(result.returncode, 1)
        self.assertIn("must be a number", result.stderr)
        self.assertEqual(calls, [])  # the running hars is left alone

    def test_without_docker_and_nobody_to_ask_it_says_how_to_get_it(self):
        # No sudo either: the installer must not try to install anything.
        result, calls = self.install(docker=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("get.docker.com", result.stderr)
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
