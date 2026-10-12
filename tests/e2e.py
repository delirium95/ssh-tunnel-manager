#!/usr/bin/env python3
"""Real OpenSSH integration checks. Uses only ephemeral loopback servers/keys."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import queue
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time


def free_port():
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        return reservation.getsockname()[1]


def receive(client, count):
    data = b""
    while len(data) < count:
        chunk = client.recv(count - len(data))
        assert chunk, "unexpected EOF"
        data += chunk
    return data


def exchange(address, family=socket.AF_INET, socks=False, target=None):
    with socket.socket(family) as client:
        client.settimeout(5)
        client.connect(address)
        if socks:
            client.sendall(b"\x05\x01\x00")
            assert receive(client, 2) == b"\x05\x00"
            client.sendall(b"\x05\x01\x00\x01" + socket.inet_aton(target[0]) + struct.pack("!H", target[1]))
            reply = receive(client, 4)
            assert reply[:2] == b"\x05\x00", reply
            if reply[3] == 1:
                receive(client, 6)
            elif reply[3] == 4:
                receive(client, 18)
            else:
                receive(client, receive(client, 1)[0] + 2)
        data = bytes(range(256)) * 1024
        client.sendall(data)
        client.shutdown(socket.SHUT_WR)
        output = b""
        while chunk := client.recv(65536):
            output += chunk
        assert output == data, f"wrong bytes through {address}"


def eventually(check, timeout=12):
    deadline = time.monotonic() + timeout
    while True:
        try:
            result = check()
            if result:
                return result
        except (OSError, AssertionError):
            pass
        if time.monotonic() >= deadline:
            raise AssertionError("condition did not become true")
        time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/boring-rs"))
    parser.add_argument("--fixture", type=Path, default=Path("target/ssh-fixture"))
    args = parser.parse_args()
    binary, fixture_binary = str(args.binary.resolve()), str(args.fixture.resolve())
    with tempfile.TemporaryDirectory(prefix="brs-", dir="/tmp") as directory:
        root = Path(directory)
        fixture_log = (root / "fixture.log").open("w+")
        fixture = subprocess.Popen([fixture_binary, str(root)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=fixture_log, text=True)
        lines = queue.Queue()
        threading.Thread(target=lambda: lines.put(fixture.stdout.readline()), daemon=True).start()
        info = json.loads(lines.get(timeout=10))
        ssh_config = root / "ssh_config"
        ssh_config.write_text(
            f"Host test-server\n  HostName 127.0.0.1\n  Port {info['port']}\n  User test\n"
            f"  IdentityFile {info['identity']}\n  IdentitiesOnly yes\n"
            f"  UserKnownHostsFile {info['known_hosts']}\n  GlobalKnownHostsFile /dev/null\n"
        )
        shim = root / "ssh"
        shim.write_text('#!/bin/sh\nexec /usr/bin/ssh -F "$TEST_SSH_CONFIG" "$@"\n')
        shim.chmod(0o700)
        runtime, config = root / "run", root / "config.toml"
        env = dict(os.environ, PATH=f"{root}:{os.environ.get('PATH', '')}",
                   BORING_CONFIG=str(config), BORING_RUNTIME_DIR=str(runtime),
                   TEST_SSH_CONFIG=str(ssh_config))
        env.pop("BORING_SOCK", None)
        env.pop("BORING_LOG_FILE", None)
        ports = {name: free_port() for name in ["local", "remote", "socks", "socks-remote"]}
        local_socket, remote_socket = str(root / "local.sock"), str(root / "remote.sock")
        echo_host, echo_port = info["echo"].rsplit(":", 1)
        target = (echo_host, int(echo_port))
        descriptions = [
            ("local", "local", str(ports["local"]), info["echo"], "data"),
            ("remote", "remote", info["echo"], str(ports["remote"]), "data"),
            ("socks", "socks", str(ports["socks"]), "", ""),
            ("socks-remote", "socks-remote", "", str(ports["socks-remote"]), ""),
            ("unix-local", "local", local_socket, info["unix"], ""),
            ("unix-remote", "remote", info["unix"], remote_socket, ""),
        ]
        text = "keep_alive = 1\nreconnect_timeout = 6\n"
        for name, mode, local, remote, group in descriptions:
            text += f"\n[[tunnels]]\nname={json.dumps(name)}\nhost='test-server'\nmode={json.dumps(mode)}\n"
            for key, value in [("local", local), ("remote", remote), ("group", group)]:
                if value:
                    text += f"{key}={json.dumps(value)}\n"
        config.write_text(text)

        def run(*words, ok=True):
            result = subprocess.run([binary, *words], env=env, capture_output=True, text=True, timeout=30)
            if ok:
                assert result.returncode == 0, f"{words}\n{result.stdout}\n{result.stderr}"
            else:
                assert result.returncode != 0, f"unexpected success: {words}"
            return result

        def states():
            return json.loads(run("list", "--json").stdout)

        def all_open():
            return len(states()) == 6 and all(entry["status"] == "open" for entry in states())

        def check_modes():
            exchange(("127.0.0.1", ports["local"]))
            exchange(("127.0.0.1", ports["remote"]))
            exchange(("127.0.0.1", ports["socks"]), socks=True, target=target)
            exchange(("127.0.0.1", ports["socks-remote"]), socks=True, target=target)
            exchange(local_socket, socket.AF_UNIX)
            exchange(remote_socket, socket.AF_UNIX)

        try:
            preview = run("open", "--all", "--dry-run")
            assert "-L" in preview.stdout and "-R" in preview.stdout and "-D" in preview.stdout
            assert not runtime.exists(), "dry-run started the manager"
            run("open", "--all")
            assert all_open()
            check_modes()
            print("PASS: all four modes, Unix sockets in both directions, binary data, half-close", flush=True)
            run("open", "local")
            with ThreadPoolExecutor(max_workers=4) as executor:
                list(executor.map(lambda _: exchange(("127.0.0.1", ports["local"])), range(4)))
            print("PASS: duplicate open is idempotent; concurrent clients", flush=True)

            run("close", "--group", "data")
            assert {entry["tunnel"]["name"] for entry in states() if entry["status"] == "open"} == {"socks", "socks-remote", "unix-local", "unix-remote"}
            run("open", "local", "remote")
            print("PASS: group selection and pattern/name union", flush=True)

            fixture.stdin.write("pause\n"); fixture.stdin.flush()
            eventually(lambda: any(entry["status"] == "reconnecting" for entry in states()))
            fixture.stdin.write("resume\n"); fixture.stdin.flush()
            eventually(all_open)
            # OpenSSH may leave a local Unix socket pathname after reconnect.
            # The manager must clean up its own listener before rebinding.
            check_modes()
            print("PASS: disconnect detection, reconnect, forwarding after recovery", flush=True)

            config.unlink()
            run("close", "--all")
            assert states() == []
            config.write_text(text)
            print("PASS: removed-config tunnels can still be listed and closed", flush=True)

            with socket.socket() as occupied:
                occupied.bind(("127.0.0.1", ports["local"]))
                occupied.listen()
                result = run("open", "local", ok=False)
                assert "bind" in result.stderr.lower() or "forward" in result.stderr.lower()
            run("close", "--all")
            print("PASS: bind failure is returned to the CLI", flush=True)

            known_hosts = Path(info["known_hosts"])
            trusted = known_hosts.read_text()
            known_hosts.write_text("")
            result = run("open", "local", ok=False)
            assert "Host key verification failed" in result.stderr
            run("close", "--all")
            known_hosts.write_text(trusted)
            print("PASS: unknown server key is rejected", flush=True)

            fixture.stdin.write("pause\n"); fixture.stdin.flush()
            result = run("open", "local", ok=False)
            assert "local" in result.stderr
            run("close", "--all")
            fixture.stdin.write("resume\n"); fixture.stdin.flush()

            # A reconnect budget that expires is visible as a terminal failure.
            config.write_text(text.replace("reconnect_timeout = 6", "reconnect_timeout = 1"))
            run("open", "local")
            fixture.stdin.write("pause\n"); fixture.stdin.flush()
            eventually(lambda: any(entry["status"] == "failed" and "reconnect timeout" in (entry["last_error"] or "") for entry in states()))
            run("close", "local")
            fixture.stdin.write("resume\n"); fixture.stdin.flush()
            config.write_text(text)
            print("PASS: initial connection errors and reconnect timeout", flush=True)

            # Serializes simultaneous startup through the daemon lock.
            run("shutdown")
            with ThreadPoolExecutor(max_workers=2) as executor:
                results = list(executor.map(lambda _: run("open", "local"), range(2)))
            assert all(result.returncode == 0 for result in results)
            exchange(("127.0.0.1", ports["local"]))
            print("PASS: concurrent daemon startup and idempotent open", flush=True)

            # Malformed IPC must not crash the daemon or allocate a huge buffer.
            with socket.socket(socket.AF_UNIX) as client:
                client.connect(str(runtime / "daemon.sock"))
                client.sendall(b"\xff\xff\xff\xff")
                size = struct.unpack("!I", receive(client, 4))[0]
                response = json.loads(receive(client, size))
                assert response["errors"]
            assert states()
            print("PASS: malformed IPC is rejected without crashing", flush=True)

            os.kill(response["pid"], signal.SIGTERM)
            eventually(lambda: not (runtime / "daemon.sock").exists())
            with socket.socket() as client:
                assert client.connect_ex(("127.0.0.1", ports["local"])) != 0
            run("open", "local")
            print("PASS: SIGTERM cleans up; a new manager can start", flush=True)

            run("shutdown")
            assert not (runtime / "daemon.sock").exists()
            with socket.socket() as client:
                assert client.connect_ex(("127.0.0.1", ports["local"])) != 0
            run("shutdown")
            print("PASS: shutdown closes listeners and is idempotent", flush=True)
        except Exception:
            print(run("list", "--json").stdout, flush=True)
            raise
        finally:
            try:
                run("shutdown")
            finally:
                fixture.terminate()
                fixture.wait(timeout=5)
                fixture_log.close()


if __name__ == "__main__":
    main()
