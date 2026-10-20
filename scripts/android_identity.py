"""Public Android identity checks; never create, rename, or print signing material."""
import os
from pathlib import Path
import re
import stat
import subprocess

PACKAGE = "fun.hackerlife.octendra"
OWNER_CERT_SHA256 = "f84d9b71f1a00f4b5e3084420e5cee3327053389983b2428d9bec71755e5539d"


def release_identity(root):
    values = {}
    for line in (Path(root) / "android/version.properties").read_text().splitlines():
        if line.strip() and not line.lstrip().startswith("#"):
            key, value = line.split("=", 1)
            if key in values:
                raise ValueError("Duplicate Android release identity field")
            values[key] = value.strip()
    if set(values) != {"versionName", "versionCode"}:
        raise ValueError("Incomplete Android release identity")
    version = values["versionName"]
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
        raise ValueError("Invalid Android release version")
    major, minor, patch = map(int, version.split("."))
    code = int(values["versionCode"])
    if minor >= 10000 or patch >= 100 or code != major * 1000000 + minor * 100 + patch:
        raise ValueError("Android version code does not match its release version")
    if not 0 < code <= 2100000000:
        raise ValueError("Android version code is out of range")
    return {"package": PACKAGE, "version": version, "version_code": code}


def owner_alias(listing):
    """Select the existing private-key entry by its reviewed public certificate."""
    matches = []
    for block in re.split(r"(?m)^Alias name: ", listing)[1:]:
        alias, _, details = block.partition("\n")
        fingerprint = re.search(r"(?m)^\s*SHA256:\s*([A-Fa-f0-9:]+)\s*$", details)
        if ("Entry type: PrivateKeyEntry" in details and fingerprint
                and fingerprint[1].replace(":", "").lower() == OWNER_CERT_SHA256):
            matches.append(alias.strip())
    if len(matches) != 1:
        raise ValueError("Expected exactly one existing owner signing key")
    return matches[0]


def configure_signing(env, directory):
    directory = Path(directory)
    password, key = directory / "password", directory / "release.p12"
    if directory.is_symlink() or not password.is_file() or not key.is_file():
        raise ValueError("Existing owner signing identity unavailable; refusing to create a replacement.")
    for path in (password, key):
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
            raise ValueError("Existing signing files must be owner-only regular files; no permissions changed.")
    env["OCTENDRA_ANDROID_STORE_PASSWORD"] = password.read_text().strip()
    env["OCTENDRA_ANDROID_KEYSTORE"] = str(key)
    result = subprocess.run(
        ["keytool", "-J-Duser.language=en", "-J-Duser.country=US", "-list", "-v",
         "-keystore", str(key), "-storetype", "PKCS12",
         "-storepass:env", "OCTENDRA_ANDROID_STORE_PASSWORD"],
        env=env, capture_output=True, text=True, check=False,
    )
    if result.returncode:
        raise ValueError("Existing signing identity could not be verified; keytool output withheld.")
    env["OCTENDRA_ANDROID_KEY_ALIAS"] = owner_alias(result.stdout)


def verify_apk(badging, signatures, identity):
    package = re.search(
        r"(?m)^package: name='([^']+)' versionCode='(\d+)' versionName='([^']+)'",
        badging,
    )
    if not package or (package[1], int(package[2]), package[3]) != (
            identity["package"], identity["version_code"], identity["version"]):
        raise ValueError("APK package/version differs from the declared release identity")
    fingerprints = re.findall(
        r"(?m)^Signer #\d+ certificate SHA-256 digest:\s*([A-Fa-f0-9]+)\s*$", signatures,
    )
    if [value.lower() for value in fingerprints] != [OWNER_CERT_SHA256]:
        raise ValueError("Owner signing identity changed")
    if "application-debuggable" in badging:
        raise ValueError("Release APK is debuggable")
