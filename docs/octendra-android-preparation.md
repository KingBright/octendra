# Octendra Android 0.11.1 preparation

This Android-only candidate uses the approved Octendra assets and application ID
`fun.hackerlife.octendra` from `octendra-migration.md`. Version code is 1101
(`major * 1000000 + minor * 100 + patch`), greater than the historical code 1.
The existing owner signing certificate remains
`f84d9b71f1a00f4b5e3084420e5cee3327053389983b2428d9bec71755e5539d`.

The new application ID is a separate Android application. It does not overwrite
`io.remotehosts.agent` or automatically inherit its data, registration, notification
permission, accessibility grant or install-source authorization. Do not uninstall or
clear the old application as a migration shortcut. An Android device needs its own
revocable enrollment configuration; no enrollment is embedded in the APK.

Build through the existing producer in the authorized independent Android worktree:

```sh
JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home \
python3 scripts/build-android.py --existing-signing --fixture-ca /path/to/existing/public/fixture_ca.pem
python3 scripts/verify-android-apk.py --apk android/app/build/outputs/apk/release/app-release.apk \
  --report android/app/build/reports/artifact-verification.json
```

`--existing-signing` refuses absent signing material rather than generating a new
identity. `--fixture-ca` reuses only a public CA in the distinct debug variant;
release packaging checks reject test trust overrides, fixture classes and signing
material. The producer runs JVM tests, release lint and debug/test/release assembly.
The build receipt binds Android source hashes and the source commit. Actual phone
and production-gateway acceptance are separate from a successful build.

The agent continues to require gateway wire protocol 2. Notifications and the
foreground service remain visible. Accessibility is required only for requested
screen interaction. Unknown-app installation authorization is needed only for the
app-driven installation path, which requests confirmation. Optional shell requires
separate local consent and authorized ADB activation; reboot or service stop can
require activation again. This candidate adds no automatic permission grants.

Installation and permission prompts remain paused until the user says ready.
At that point, use a uniquely identified phone and normal installer confirmation,
then verify installed bytes, launch and permission state. Same-package, same-signer
replacement must be tested independently before claiming unattended future updates.
Build success does not authorize publication; the existing publisher still requires
matching runtime acceptance for the exact APK.
