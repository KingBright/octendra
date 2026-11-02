#!/usr/bin/env python3
"""Build on an owner's development machine; signing secrets never enter source or command arguments."""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, time
import release_current
import android_identity
ROOT = pathlib.Path(__file__).resolve().parents[1]

def digest(path):
    h=hashlib.sha256()
    with path.open('rb') as stream:
        for data in iter(lambda:stream.read(65536), b''): h.update(data)
    return h.hexdigest()

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--publish', action='store_true')
    parser.add_argument('--existing-signing', action='store_true', help='Existing signing is always required; retained for current build invocations.')
    parser.add_argument('--signing-directory', type=pathlib.Path, default=pathlib.Path.home()/'.local/share/octendra-code/android-signing',
                        help='Existing owner-private signing directory. Never create, rename, or chmod signing material.')
    parser.add_argument('--fixture-ca', type=pathlib.Path, help='Reuse an existing public debug fixture CA without generating private fixture material.')
    signing=parser.add_mutually_exclusive_group()
    signing.add_argument('--prepare-signing', action='store_true', help='Build unsigned for signing on the machine holding the existing owner key.')
    signing.add_argument('--signed-apk', type=pathlib.Path, help='Finish a prepared build with its externally signed APK; no key transfer.')
    args=parser.parse_args()
    if args.publish:
        parser.error('Build first, run real Android acceptance, then use scripts/publish-android.py --acceptance RECEIPT. Build success alone cannot publish.')
    sdk=pathlib.Path(os.environ.get('ANDROID_HOME',str(pathlib.Path.home()/'Library/Android/sdk')))
    env=os.environ.copy(); env['ANDROID_HOME']=str(sdk); env['ANDROID_SDK_ROOT']=str(sdk)
    snapshot=release_current.snapshot_evidence(None, ROOT)
    build_revision=release_current.runtime_revision(snapshot)
    env['OCTENDRA_BUILD_REVISION']=build_revision
    env.setdefault('JAVA_HOME','/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home')
    env['PATH']=str(pathlib.Path(env['JAVA_HOME'])/'bin')+os.pathsep+env.get('PATH','')
    identity=android_identity.release_identity(ROOT)
    reports=ROOT/'android/app/build/reports'; reports.mkdir(parents=True,exist_ok=True)
    request=reports/'signing-request.json'
    unsigned=ROOT/'android/app/build/outputs/apk/release/app-release-unsigned.apk'
    apk=ROOT/'android/app/build/outputs/apk/release/app-release.apk'
    if args.signed_apk is not None:
        prepared=json.loads(request.read_text())
        if prepared['snapshot_id'] != snapshot['snapshot_id'] or prepared['unsigned_sha256'] != digest(unsigned):
            raise SystemExit('Prepared Android build or source changed; preserve the request and rebuild.')
        android_identity.verify_signed_payload(unsigned,args.signed_apk)
    elif not args.prepare_signing:
        try:
            android_identity.configure_signing(env,args.signing_directory)
        except ValueError as error:
            raise SystemExit(str(error)) from None
    else:
        for name in ('OCTENDRA_ANDROID_KEYSTORE','OCTENDRA_ANDROID_STORE_PASSWORD','OCTENDRA_ANDROID_KEY_ALIAS'):
            env.pop(name,None)
    if args.signed_apk is None and args.fixture_ca is not None:
        cert=args.fixture_ca.resolve(strict=True)
        if not cert.is_file(): raise SystemExit('Public fixture CA unavailable')
        raw=ROOT/'android/app/src/debug/res/raw'; raw.mkdir(parents=True,exist_ok=True)
        if cert != (raw/'fixture_ca.pem').resolve(): shutil.copy2(cert,raw/'fixture_ca.pem')
    elif args.signed_apk is None:
        fixture=ROOT/'android/build/integration-tls'
        fixture.mkdir(parents=True,exist_ok=True)
        cert=fixture/'fixture_ca.pem'; tlskey=fixture/'fixture.key'
        if not cert.exists():
            subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(tlskey),'-out',str(cert),'-days','30',
              '-subj','/CN=Octendra Local Integration CA','-addext','subjectAltName=DNS:localhost,IP:127.0.0.1',
              '-addext','basicConstraints=critical,CA:TRUE'],check=True,capture_output=True)
            tlskey.chmod(0o600)
        raw=ROOT/'android/app/src/debug/res/raw'; raw.mkdir(parents=True,exist_ok=True)
        shutil.copy2(cert,raw/'fixture_ca.pem')
    if args.signed_apk is None:
        subprocess.run(['./gradlew','--no-daemon','testDebugUnitTest','lintRelease','assembleDebug','assembleDebugAndroidTest','assembleRelease'],cwd=ROOT/'android',env=env,check=True)
    release_current.check_inputs(ROOT,snapshot)
    if args.prepare_signing:
        request.write_text(json.dumps({'snapshot_id':snapshot['snapshot_id'],'build_revision':build_revision,'unsigned_sha256':digest(unsigned)},indent=2)+'\n')
        print(json.dumps({'apk':str(unsigned.relative_to(ROOT)),'sha256':digest(unsigned),'signed_release':False,'next_action':'sign with the existing owner key, then --signed-apk PATH'}))
        return
    candidate=args.signed_apk or apk
    if not candidate.exists(): raise SystemExit('signed release APK missing')
    tools=sdk/'build-tools/36.1.0'
    verify=subprocess.run([str(tools/'apksigner'),'verify','--verbose','--print-certs',str(candidate)],env=env,check=True,capture_output=True,text=True).stdout
    badging=subprocess.run([str(tools/'aapt'),'dump','badging',str(candidate)],check=True,capture_output=True,text=True).stdout
    android_identity.verify_apk(badging,verify,identity)
    if candidate.resolve() != apk.resolve(): shutil.copy2(candidate,apk)
    declared=[p for p in (ROOT/'android').rglob('*') if p.is_file() and not any(part in {'build','.gradle','.kotlin','.idea'} for part in p.relative_to(ROOT/'android').parts) and p.name!='local.properties']
    sources={str(p.relative_to(ROOT)):digest(p) for p in sorted(declared)}
    report={**identity,'build_revision':build_revision,'source_commit':subprocess.run(['git','rev-parse','HEAD'],cwd=ROOT,check=True,capture_output=True,text=True).stdout.strip(),'apk_sha256':digest(apk),'bytes':apk.stat().st_size,'built_at':int(time.time()),
      'source_hashes':sources,'signing_certificate_sha256':android_identity.OWNER_CERT_SHA256,'signature_verified':True,
      'badging':badging,'runtime_acceptance':'separate receipt required','signing_material_in_package':False}
    (reports/'delivery.json').write_text(json.dumps(report,indent=2,ensure_ascii=False)+'\n')
    print(json.dumps({'apk':str(apk.relative_to(ROOT)),'sha256':report['apk_sha256'],'bytes':report['bytes'],'signed_release':True}))
if __name__=='__main__':main()
