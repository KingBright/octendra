#!/usr/bin/env python3
"""Run JVM unit tests with public debug fixtures; never signs or installs an APK."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def prepare_public_fixture():
    destination = ROOT / "android/app/src/debug/res/raw/fixture_ca.pem"
    if destination.exists():
        if destination.is_symlink() or not destination.is_file():
            raise SystemExit("Expected a regular public fixture certificate")
        return
    existing = ROOT / "android/build/integration-tls/fixture_ca.pem"
    with tempfile.TemporaryDirectory(prefix="octendra-unit-public-ca-") as temporary:
        public = Path(temporary) / "fixture_ca.pem"
        if existing.is_file() and not existing.is_symlink():
            shutil.copyfile(existing, public)  # Public certificate only; no key access.
        else:
            # Resource compilation needs a real certificate, not its private key.
            # Discard this artificial key; runtime TLS integration prepares its own pair.
            subprocess.run([
                "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", os.devnull, "-out", str(public), "-days", "7",
                "-subj", "/CN=Octendra JVM Unit Fixture",
                "-addext", "basicConstraints=critical,CA:TRUE",
            ], check=True, capture_output=True)
        subprocess.run(["openssl", "x509", "-in", str(public), "-noout"],
                       check=True, capture_output=True)
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("xb") as target:
            target.write(public.read_bytes())


def main():
    prepare_public_fixture()
    env = os.environ.copy()
    jdk = Path("/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home")
    if not env.get("JAVA_HOME") and (jdk / "bin/java").is_file():
        env["JAVA_HOME"] = str(jdk)
    sdk = Path.home() / "Library/Android/sdk"
    if not env.get("ANDROID_HOME") and sdk.is_dir():
        env["ANDROID_HOME"] = str(sdk)
    for key in ("OCTENDRA_ANDROID_KEYSTORE", "OCTENDRA_ANDROID_STORE_PASSWORD",
                "OCTENDRA_ANDROID_KEY_ALIAS"):
        env.pop(key, None)
    return subprocess.run(["./gradlew", "--offline", "--no-daemon", "testDebugUnitTest"],
                          cwd=ROOT / "android", env=env).returncode


if __name__ == "__main__":
    raise SystemExit(main())
