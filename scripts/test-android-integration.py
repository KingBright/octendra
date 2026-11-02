#!/usr/bin/env python3
"""Real Rust Gateway + TLS + MCP + Android emulator acceptance. Never touches the live gateway.

Requires the separate debug APK and its test APK built by build-android.py. The shipped
release APK has neither fixture Activity, instrumentation, nor test CA configuration.
"""
import argparse, base64, hashlib, http.cookiejar, json, os, pathlib, re, shlex, socket, ssl, subprocess, sys, time, urllib.request, uuid
from release_client import Client, NoRedirect
ROOT=pathlib.Path(__file__).resolve().parents[1]
PACKAGE='fun.hackerlife.octendra.debug'

def main():
    p=argparse.ArgumentParser();p.add_argument('--serial',required=True);p.add_argument('--gateway-binary',required=True,type=pathlib.Path)
    p.add_argument('--install-fixture-apk',type=pathlib.Path,default=ROOT/'android/app/build/outputs/apk/release/app-release.apk',
                   help='Existing owner-signed APK used only to test file transfer/installation; not evidence that the tested debug source was signed.')
    p.add_argument('--adb',default=str(pathlib.Path.home()/'Library/Android/sdk/platform-tools/adb'));a=p.parse_args()
    if not re.fullmatch(r'emulator-\d+',a.serial):raise SystemExit('dedicated Android emulator required; physical phones are never test fixtures')
    try:
        from PIL import Image
    except ImportError:
        raise SystemExit('Android acceptance needs Pillow in the test Python environment; install it in a virtual environment first.') from None
    adb=[a.adb,'-s',a.serial]
    def run(args,**kwargs):return subprocess.run(args,check=True,capture_output=True,timeout=120,**kwargs)
    def shell(*args):return run(adb+['shell',*args],text=True).stdout.strip()
    if shell('getprop','ro.kernel.qemu')!='1':raise SystemExit('not an emulator')
    runid=str(uuid.uuid4()); base=ROOT/'android/build/integration'/runid;base.mkdir(parents=True,mode=0o700)
    checks=[]; children=[];handles=[];client=None;instrument=None;reverse_ready=False
    def checked(name,truth,**facts):
        if not truth:raise AssertionError(name)
        checks.append(dict(name=name,passed=True,**facts));print('PASS '+name,flush=True)
    def spawn(args,filename):
        h=(base/filename).open('wb');handles.append(h);proc=subprocess.Popen(args,stdout=h,stderr=subprocess.STDOUT,start_new_session=True);children.append(proc);return proc
    def freeport():
        with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
    tlsport,innerport=freeport(),freeport();origin=f'https://localhost:{tlsport}'
    gateway=a.gateway_binary.resolve(strict=True);cfg=base/'gateway.json';password=base/'owner-password';agent=base/'android-device.json'
    report={'run_id':runid,'environment':'isolated Rust gateway and dedicated Android emulator','checks':checks,'release_apk_tested_as_install_target':False,'physical_phone_tested':False,'passed':False}
    try:
        version=run([str(gateway),'--version'],text=True).stdout.strip();report['gateway_version']=version
        run([str(gateway),'init-gateway','--config',str(cfg),'--public-url',origin,'--bind',f'127.0.0.1:{innerport}','--state-dir',str(base/'gateway-state'),'--password-file',str(password),'--owner','android-fixture'])
        run([str(gateway),'enroll','--gateway-config',str(cfg),'--agent-config',str(agent),'--name','Android Integration Emulator','--state-dir','/android/private','--root','/android/shared','--allow-write','--allow-exec','--shell','/system/bin/sh'])
        tls=ROOT/'android/build/integration-tls';cert=tls/'fixture_ca.pem';key=tls/'fixture.key'
        spawn([str(gateway),'gateway','--config',str(cfg)],'gateway.log')
        spawn([sys.executable,str(ROOT/'scripts/android-fixture-tls-proxy.py'),'--listen-port',str(tlsport),
               '--upstream-port',str(innerport),'--cert',str(cert),'--key',str(key)],'tls.log')
        context=ssl.create_default_context(cafile=str(cert))
        for _ in range(100):
            try:
                with urllib.request.urlopen(origin+'/healthz',context=context,timeout=2) as r:health=json.load(r)
                break
            except Exception:time.sleep(.2)
        else:raise RuntimeError('isolated gateway did not start; inspect fixture logs')
        checked('real_gateway_wire_protocol',health.get('wire_protocol')==2)
        run(adb+['reverse',f'tcp:{tlsport}',f'tcp:{tlsport}']);reverse_ready=True
        run(adb+['install','-r',str(ROOT/'android/app/build/outputs/apk/debug/app-debug.apk')])
        run(adb+['install','-r','-t',str(ROOT/'android/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk')])
        result=run(adb+['shell','am','instrument','-w',PACKAGE+'.test/fun.hackerlife.octendra.AgentInstrumentation'],text=True).stdout
        (base/'runtime-checks.txt').write_text(result)
        runtime_checks = re.search(r'(\d+) runtime checks passed', result)
        checked('android_keystore_sqlite_runtime_checks',runtime_checks is not None and int(runtime_checks[1]) >= 10)
        run(adb+['shell','pm','grant',PACKAGE,'android.permission.POST_NOTIFICATIONS'])
        previous=shell('settings','get','secure','enabled_accessibility_services')
        services=[] if previous in ('','null') else previous.split(':')
        component=PACKAGE+'/fun.hackerlife.octendra.AccessService'
        services=[item for item in services if item!=component]
        # Instrumentation restarts the target process. Enable this fixture's
        # accessibility service AFTER that restart, preserving unrelated services.
        if services:shell('settings','put','secure','enabled_accessibility_services',':'.join(services))
        else:shell('settings','delete','secure','enabled_accessibility_services')
        data=json.loads(agent.read_text());device=data['device_id']
        run(adb+['shell','run-as',PACKAGE,'mkdir','-p','files'])
        run(adb+['shell','run-as',PACKAGE,'sh','-c',shlex.quote('cat > files/integration-config.json')],input=agent.read_bytes())
        instrument=spawn(adb+['shell','am','instrument','-w','-e','mode','fixture',PACKAGE+'.test/fun.hackerlife.octendra.AgentInstrumentation'],'fixture-instrumentation.log')
        client=Client(origin,password_file=password,transport='legacy')
        client.opener=urllib.request.build_opener(NoRedirect(),urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()),urllib.request.HTTPSHandler(context=context),urllib.request.ProxyHandler({}))
        client.login()
        selected=None
        for _ in range(90):
            rows=client.tool('devices_list',{}).get('devices',[])
            selected=next((d for d in rows if d['device_id']==device and d.get('online')),None)
            if selected:break
            if instrument.poll() is not None:raise RuntimeError('fixture instrumentation exited; inspect its log')
            time.sleep(1)
        checked('android_registered_online_via_tls',selected is not None)
        registered=next(d for d in json.loads(cfg.read_text())['devices'] if d['id']==device)
        checked('unified_ui_reuses_existing_device_scopes',not any(s.startswith('desktop:') for s in registered['scopes']))
        root=selected['capabilities']['roots'][0]
        ws=client.tool('workspace_open',{'device_id':device,'root':root,'idempotency_key':runid+'-workspace'})['workspace']['id']
        counter=0
        def cmd(name,args=None,key=None):
            nonlocal counter;counter+=1
            arguments={'workspace_id':ws,'command':'android '+name+((' '+json.dumps(args,ensure_ascii=False)) if args is not None else ''),'idempotency_key':key or runid+'-'+str(counter),'timeout_seconds':240}
            value=client.tool('terminal_exec',arguments)
            parsed=json.loads(value['output'])
            (base/f'{counter:03}-{name}.json').write_text(json.dumps(parsed,indent=2,ensure_ascii=False)+'\n')
            return parsed,value
        shell('settings','put','secure','enabled_accessibility_services',':'.join(services+[component]))
        shell('settings','put','secure','accessibility_enabled','1')
        for _ in range(30):
            status,_=cmd('status')
            if status.get('accessibility'):break
            time.sleep(.5)
        checked('accessibility_capability_live',status['accessibility'] is True and status['shell_connected'] is False)
        missing,_=cmd('shell',{'command':'id'});checked('shell_denied_before_local_activation',missing.get('error')=='shell_activation_required')
        launched=shell('am','start','-W','-n',PACKAGE+'/fun.hackerlife.octendra.ProbeActivity')
        (base/'probe-launch.txt').write_text(launched)
        node=None;system_waits=0
        for _ in range(20):
            obs,_=cmd('observe')
            node=next((n for n in obs.get('nodes',[]) if n.get('description')=='fixture-input'),None)
            if node is not None and obs.get('package')==PACKAGE:break
            # The dedicated emulator may show its own System UI ANR after a
            # heavy cold build. Observe and press only its explicit Wait button,
            # through the real agent, rather than dismissing arbitrary dialogs.
            if obs.get('package')=='android' and any(n.get('text')=="System UI isn't responding" for n in obs.get('nodes',[])) and system_waits<3:
                wait=next((n for n in obs['nodes'] if n.get('resource_id')=='android:id/aerr_wait'),None)
                if wait:
                    outcome,_=cmd('click',{'observation_id':obs['observation_id'],'node_id':wait['id']})
                    demand_wait=outcome.get('accepted') is True
                    if not demand_wait:raise AssertionError('observed_system_dialog_wait_rejected')
                    system_waits+=1;report['emulator_system_ui_wait_dialogs']=system_waits
            time.sleep(.5)
        (base/'observed-screen.json').write_text(json.dumps(obs,indent=2,ensure_ascii=False)+'\n')
        checked('fixture_screen_in_foreground',node is not None and obs.get('package')==PACKAGE)
        checked('structured_screen_tree',len(obs.get('nodes',[]))>=4)
        checked('password_nodes_redacted',any(n.get('password') is True and n.get('text')=='<redacted>' for n in obs['nodes']) and 'fixture-sensitive-value' not in json.dumps(obs))
        for attempt in range(5):
            value,_=cmd('text',{'observation_id':obs['observation_id'],'node_id':node['id'],'text':'你好 Octendra'})
            if value.get('accepted') is True:break
            if value.get('error') not in ('node_changed_observe_again','screen_changed_observe_again'):break
            # These are explicit pre-action refusals, not uncertain executions.
            # Acquire a NEW observation and operation; never replay a lost result.
            time.sleep(.3);obs,_=cmd('observe')
            node=next(n for n in obs['nodes'] if n.get('description')=='fixture-input')
        checked('unicode_text_action_accepted',value.get('accepted') is True)
        stale,_=cmd('click',{'observation_id':obs['observation_id'],'node_id':node['id']})
        checked('stale_observation_rejected',stale.get('error') in ('observation_required','stale_observation','node_changed_observe_again'))
        for attempt in range(5):
            obs,_=cmd('observe');button=next(n for n in obs['nodes'] if (n.get('text') or '').casefold()=='apply fixture text' and n.get('class')=='android.widget.Button')
            value,_=cmd('click',{'observation_id':obs['observation_id'],'node_id':button['id']})
            if value.get('accepted') is True:break
            if value.get('error') not in ('node_changed_observe_again','screen_changed_observe_again'):break
            time.sleep(.3)
        if value.get('accepted') is not True:raise AssertionError('fixture_button_click_rejected: '+json.dumps(value))
        for _ in range(10):
            obs,_=cmd('observe')
            if any(n.get('text')=='已确认：你好 Octendra' for n in obs.get('nodes',[])):break
            time.sleep(.2)
        checked('click_verified_by_changed_screen',any(n.get('text')=='已确认：你好 Octendra' for n in obs.get('nodes',[])))
        shot,_=cmd('screenshot');checked('screenshot_created',shot.get('size',0)>0 and shot.get('secure_windows_bypassed') is False)
        artifact=client.tool('file_download',{'workspace_id':ws,'path':shot['path'],'idempotency_key':runid+'-screenshot-download'})
        png=client.read_artifact(artifact);(base/'probe.png').write_bytes(png)
        checked('binary_screenshot_download_verified',png.startswith(b'\x89PNG\r\n\x1a\n') and hashlib.sha256(png).hexdigest()==artifact['sha256'])
        import io
        def content_extrema(raw):
            with Image.open(io.BytesIO(raw)) as image:
                image.load();width,height=image.size
                if width<100 or height<100:raise AssertionError('invalid screenshot dimensions')
                box=(width//10,height*15//100,width*9//10,height*85//100)
                return image.size,box,image.convert('RGB').crop(box).getextrema()
        normal_size,normal_box,normal_extrema=content_extrema(png)
        checked('ordinary_screen_has_visible_pixels',max(high for low,high in normal_extrema)>32)
        # Shared UI tools use the real MCP boundary and native image blocks,
        # without an Android command string, workspace or caller-managed lease.
        def ui(name,arguments):
            # This fixture accepts the phone's existing display adapter, not a
            # provisioned isolated profile/VM. Select that boundary explicitly.
            if name=='ui_observe':arguments={'context':'host',**arguments}
            response=client.rpc('tools/call',{'name':name,'arguments':arguments})
            value=response.get('structuredContent',{})
            operation=value.get('operation_id');end=time.monotonic()+60
            while value.get('pending'):
                if not operation or time.monotonic()>end:raise AssertionError('ui_operation_observation_timeout')
                response=client.rpc('tools/call',{'name':'operation_get','arguments':{'operation_id':operation,'wait_ms':5000}})
                value=response.get('structuredContent',{})
            images=[block for block in response.get('content',[]) if block.get('type')=='image']
            if images:
                image=images[0];raw=base64.b64decode(image['data'],validate=True)
                with Image.open(io.BytesIO(raw)) as decoded:
                    decoded.load()
                    metadata=value.get('observation',value)['image']
                    checked('unified_ui_native_image_dimensions',list(decoded.size)==[metadata['width'],metadata['height']],width=decoded.width,height=decoded.height)
                checked('unified_ui_base64_not_repeated_as_text',all(image['data'] not in block.get('text','') for block in response.get('content',[])))
                (base/'unified-ui.jpg').write_bytes(raw)
            return value
        observed=ui('ui_observe',{'device':device})
        checked('unified_ui_observe_phone',observed.get('view')=='display:0' and len(observed.get('elements',[]))>=4)
        checked('unified_ui_password_redaction','fixture-sensitive-value' not in json.dumps(observed))
        editor=next(n for n in observed['elements'] if n.get('description')=='fixture-input')
        filled=ui('ui_act',{'device':device,'observation_id':observed['observation_id'],
            'action':{'type':'fill','target':{'ref':editor['ref']},'text':'统一界面'}})
        checked('unified_ui_fill_replaces',any(n.get('text')=='统一界面' for n in filled.get('observation',{}).get('elements',[])))
        observed=filled['observation'];editor=next(n for n in observed['elements'] if n.get('description')=='fixture-input')
        insertion={'device':device,'observation_id':observed['observation_id'],'action':{'type':'type_text','target':{'ref':editor['ref']},'text':'中文🙂'},'idempotency_key':runid+'-ui-insert'}
        inserted=ui('ui_act',insertion)
        text=next(n.get('text','') for n in inserted.get('observation',{}).get('elements',[]) if n.get('description')=='fixture-input')
        checked('unified_ui_unicode_insert_preserves_existing','统一界面' in text and '中文🙂' in text and len(text)==7)
        duplicate=ui('ui_act',insertion)
        checked('unified_ui_journal_deduplicates_input',duplicate.get('operation_id')==inserted.get('operation_id'))
        observed=ui('ui_observe',{'device':device})
        editor=next(n for n in observed['elements'] if n.get('description')=='fixture-input')
        time.sleep(31)  # Prove thinking time does not expire a unified UI session.
        long_think=ui('ui_act',{'device':device,'observation_id':observed['observation_id'],
            'action':{'type':'fill','target':{'ref':editor['ref']},'text':'长思考后仍可操作'}})
        checked('unified_ui_no_thirty_second_thinking_lease',any(n.get('text')=='长思考后仍可操作' for n in long_think.get('observation',{}).get('elements',[])))
        observed=long_think['observation']
        button=next(n for n in observed['elements'] if (n.get('text') or '').casefold()=='apply fixture text' and n.get('class')=='android.widget.Button')
        left,top,right,bottom=button['bounds']
        clicked=ui('ui_act',{'device':device,'observation_id':observed['observation_id'],
            'action':{'type':'click','target':{'x':(left+right)/2,'y':(top+bottom)/2}}})
        checked('unified_ui_scaled_image_click_has_real_effect',any(n.get('text')=='已确认：长思考后仍可操作' for n in clicked.get('observation',{}).get('elements',[])))
        observed=ui('ui_observe',{'device':device});cmd('observe')
        refused=ui('ui_act',{'device':device,'observation_id':observed['observation_id'],'action':{'type':'go_home'}})
        checked('unified_ui_stale_returns_fresh_without_input',refused.get('error')=='stale_observation' and refused.get('input_sent') is False and bool(refused.get('observation',{}).get('observation_id')))
        try:
            imported=client.tool('file_upload',{'workspace_id':ws,'path':'roundtrip/probe.png','file':{'file_id':'fixture-'+runid,'download_url':artifact['download_url']},'sha256':artifact['sha256'],'idempotency_key':runid+'-upload'})
            checked('binary_file_upload_verified',imported.get('sha256')==artifact['sha256'] and imported.get('atomic') is True)
            roundtrip=client.tool('file_download',{'workspace_id':ws,'path':'roundtrip/probe.png','idempotency_key':runid+'-roundtrip-download'})
            checked('file_roundtrip_identical',client.read_artifact(roundtrip)==png)
            conflict=False
            try:
                client.tool('file_upload',{'workspace_id':ws,'path':'roundtrip/probe.png','file':{'file_id':'fixture-'+runid,'download_url':artifact['download_url']},'sha256':artifact['sha256'],'idempotency_key':runid+'-conflicting-upload'})
            except RuntimeError as exc:
                conflict='version_conflict' in str(exc)
            checked('upload_refuses_unapproved_overwrite',conflict)
            preserved=client.tool('file_download',{'workspace_id':ws,'path':'roundtrip/probe.png','idempotency_key':runid+'-after-conflict'})
            checked('failed_upload_preserved_original',preserved['sha256']==artifact['sha256'] and client.read_artifact(preserved)==png)
        except Exception as exc:
            report.setdefault('failed_checks',[]).append({'name':'file_upload_roundtrip','error':str(exc)})
            print('FAIL file_upload_roundtrip: '+str(exc),flush=True)
            # Keep testing the independent shell/install lane, but never mark
            # this run passed or publish it while any mandatory check failed.
        helper_log='/data/local/tmp/octendra-helper-'+runid+'.log'
        run(['python3',str(ROOT/'scripts/android-activate-shell.py'),'--adb',a.adb,'--serial',a.serial,'--package',PACKAGE,'--diagnostic-log',helper_log])
        for _ in range(20):
            status,_=cmd('status')
            if status.get('shell_connected'):break
            time.sleep(.3)
        helper_diagnostic=shell('cat',helper_log)
        (base/'helper-startup.txt').write_text(helper_diagnostic)
        checked('builtin_shell_bridge_activated',status.get('shell_connected') is True and status.get('shell_uid')==2000)
        identity,_=cmd('shell',{'command':'id'});checked('real_adb_shell_uid',identity.get('exit_code')==0 and 'uid=2000' in identity.get('output',''))
        background=ui('ui_observe',{'device':device,'view':'background'})
        checked('unified_ui_background_has_own_display',background.get('source',{}).get('kind')=='ai_display' and background['source']['display_id']>0)
        background_view=background['view'];generation=background['source']['generation']
        launched=ui('ui_act',{'device':device,'observation_id':background['observation_id'],'action':{'type':'launch_app','app':'com.android.settings'}})
        checked('unified_ui_background_launch_accepted',launched.get('accepted') is True)
        for _ in range(10):
            background=ui('ui_observe',{'device':device,'view':background_view})
            if background.get('package')=='com.android.settings':break
            time.sleep(.3)
        checked('unified_ui_background_semantic_tree',background.get('package')=='com.android.settings' and len(background.get('elements',[]))>3)
        main,_=cmd('observe')
        checked('unified_ui_background_did_not_move_main_app',main.get('package')==PACKAGE)
        # Reading main must not invalidate a separate background observation.
        row=next((n for n in background['elements'] if n.get('text')=='Network & internet'),None)
        if row is None:row=next(n for n in background['elements'] if n.get('clickable') and n.get('enabled') and n.get('visible'))
        def semantic_screen(value):return [(n.get('text'),n.get('description'),n.get('class')) for n in value.get('elements',[])]
        before=semantic_screen(background)
        offscreen=ui('ui_act',{'device':device,'observation_id':background['observation_id'],'action':{'type':'click','target':{'ref':row['ref']}}})
        checked('unified_ui_display_observations_are_independent',offscreen.get('accepted') is True)
        for _ in range(10):
            after=ui('ui_observe',{'device':device,'view':background_view})
            if semantic_screen(after)!=before:break
            time.sleep(.2)
        checked('unified_ui_background_click_has_real_effect',semantic_screen(after)!=before)
        back_state=after
        for _ in range(2):
            back=ui('ui_act',{'device':device,'observation_id':back_state['observation_id'],'action':{'type':'navigate_back'}})
            if back.get('error')!='stale_observation' or back.get('input_sent') is not False:break
            back_state=back['observation']
        checked('unified_ui_background_native_key_delivered',back.get('accepted') is True)
        main,_=cmd('observe');checked('unified_ui_background_click_kept_main',main.get('package')==PACKAGE)
        background=ui('ui_observe',{'device':device,'view':background_view})
        protected=ui('ui_act',{'device':device,'observation_id':background['observation_id'],'action':{'type':'launch_app','app':PACKAGE}})
        checked('unified_ui_main_app_not_stolen_by_background',protected.get('error')=='app_in_use_on_main' and protected.get('input_sent') is False)
        report['unified_ui_background']={'view':background_view,'generation':generation,'main_app_preserved':True,'real_time_video_tested':False}
        original=ui('ui_observe',{'device':device,'view':background_view})
        uid=int(re.search(r'uid:(\d+)',shell('cmd','package','list','packages','-U',PACKAGE))[1])
        helper_pids=[]
        for line in shell('ps','-A','-o','PID,ARGS').splitlines():
            match=re.fullmatch(r'\s*(\d+)\s+.*\bfun\.hackerlife\.octendra\.ShellMain\s+octendra-\d+-[a-f0-9]{32}\s+(\d+)\s*',line)
            if match and int(match[2])==uid:helper_pids.append(match[1])
        checked('unified_ui_recovery_helper_is_owned_fixture',len(helper_pids)==1)
        shell('kill','-TERM',helper_pids[0])
        for _ in range(20):
            status,_=cmd('status')
            if not status.get('shell_connected'):break
            time.sleep(.1)
        checked('unified_ui_helper_loss_observed',status.get('shell_connected') is False)
        run(['python3',str(ROOT/'scripts/android-activate-shell.py'),'--adb',a.adb,'--serial',a.serial,'--package',PACKAGE,'--diagnostic-log',helper_log])
        recovered=ui('ui_act',{'device':device,'observation_id':original['observation_id'],'action':{'type':'go_home'}})
        checked('unified_ui_helper_recovery_never_replays_old_action',recovered.get('error')=='stale_observation' and recovered.get('input_sent') is False and
                recovered.get('observation',{}).get('source',{}).get('generation') not in (None,generation))
        reconnect,_=cmd('reconnect');checked('unified_ui_network_generation_recovered',reconnect.get('network_generation',0)>0)
        duplicate=ui('ui_act',insertion)
        checked('unified_ui_retry_after_reconnect_kept_original_operation',duplicate.get('operation_id')==inserted.get('operation_id'))
        current=ui('ui_observe',{'device':device})
        checked('unified_ui_retry_did_not_reinsert_text',any(n.get('text')=='长思考后仍可操作' for n in current.get('elements',[])))
        source='/data/local/tmp/octendra-counter-'+runid;repeatkey=runid+'-repeat'
        first,firstmeta=cmd('shell',{'command':'echo once >> '+source},key=repeatkey)
        second,secondmeta=cmd('shell',{'command':'echo once >> '+source},key=repeatkey)
        counted,_=cmd('shell',{'command':'wc -l < '+source})
        checked('mutating_idempotency_verified',firstmeta['terminal_id']==secondmeta['terminal_id'] and counted.get('output','').strip()=='1')
        pulled,_=cmd('pull',{'source':source,'path':'fixtures/counter.txt'})
        checked('shell_file_pull',pulled.get('exit_code')==0 and pulled.get('size')==5)
        text=client.tool('code_read',{'workspace_id':ws,'requests':[{'path':'fixtures/counter.txt','start_line':1,'end_line':3}]})
        checked('shared_file_text_read',text['ranges'][0]['text'].strip()=='once')
        apk=a.install_fixture_apk.resolve(strict=True);apkhash=hashlib.sha256(apk.read_bytes()).hexdigest()
        remote_apk=root+'/fixtures/release.apk'
        run(adb+['shell','run-as',PACKAGE,'sh','-c',shlex.quote('cat > '+shlex.quote(remote_apk))],input=apk.read_bytes())
        installed,_=cmd('install',{'path':'fixtures/release.apk','sha256':apkhash})
        checked('release_apk_installed_by_shell_bridge',installed.get('installed') is True and installed.get('package')=='fun.hackerlife.octendra')
        installed_path=next(line.removeprefix('package:') for line in shell('pm','path','fun.hackerlife.octendra').splitlines() if line.endswith('/base.apk'))
        installed_hash,_=cmd('shell',{'command':'sha256sum '+shlex.quote(installed_path)})
        checked('installed_release_apk_bytes_match',installed_hash.get('exit_code')==0 and installed_hash.get('output','').split()[0]==apkhash)
        report['release_apk_tested_as_install_target']=True;report['release_apk_sha256']=apkhash
        report['install_fixture_apk']=str(apk);report['install_fixture_is_debug_source_signing_evidence']=False
        launched,_=cmd('launch',{'package':'fun.hackerlife.octendra'})
        checked('release_launch_requested_through_gateway',launched.get('accepted') is True)
        time.sleep(1)
        running=shell('pidof','fun.hackerlife.octendra');checked('signed_release_launch_smoke',bool(running))
        # The status page is intentionally capturable; only the expanded secret
        # editor is secure. Open that actual boundary through the existing agent.
        editor_opened=False
        for _ in range(5):
            observation,_=cmd('observe')
            edit=next((node for node in observation.get('nodes',[]) if node.get('text')=='编辑连接配置'),None)
            if edit is not None and observation.get('package')=='fun.hackerlife.octendra':
                opened,_=cmd('click',{'observation_id':observation['observation_id'],'node_id':edit['id']})
                if opened.get('accepted') is True:
                    editor_opened=True;break
            time.sleep(.3)
        checked('configuration_editor_opened_through_gateway',editor_opened)
        time.sleep(.3)
        for _ in range(10):
            result,_=cmd('screenshot')
            if str(result.get('error','')).startswith('screenshot_denied') or result.get('package')=='fun.hackerlife.octendra':break
            time.sleep(.3)
        if str(result.get('error','')).startswith('screenshot_denied'):
            checked('secure_configuration_window_not_captured',True,protection='explicit_platform_denial')
        else:
            # Whole-display capture may succeed while the secure window is
            # replaced by black pixels. Verify the actual returned bytes, not
            # just an API return code or the agent's own protection claim.
            checked('signed_release_is_foreground',result.get('package')=='fun.hackerlife.octendra')
            secure_artifact=client.tool('file_download',{'workspace_id':ws,'path':result['path'],'idempotency_key':runid+'-secure-screenshot-download'})
            secure_png=client.read_artifact(secure_artifact)
            checked('secure_capture_checksum_verified',hashlib.sha256(secure_png).hexdigest()==secure_artifact['sha256'])
            (base/'secure-window.png').write_bytes(secure_png)
            size,box,extrema=content_extrema(secure_png)
            checked('secure_configuration_window_not_captured',size==normal_size and all(high<=3 for low,high in extrema),
                    protection='platform_redacted_pixels',content_box=box,channel_extrema=extrema)
        run(adb+['shell','run-as',PACKAGE,'touch','files/integration-stop']);instrument.wait(timeout=30)
        time.sleep(.5)
        services=shell('dumpsys','activity','services',PACKAGE)
        checked('local_stop_removed_foreground_service','ServiceRecord' not in services or 'fun.hackerlife.octendra.AgentService' not in services)
        report['passed']=not bool(report.get('failed_checks'))
    finally:
        if client:
            try:client.close()
            except Exception:pass
        if instrument and instrument.poll() is None:
            try:run(adb+['shell','run-as',PACKAGE,'touch','files/integration-stop']);instrument.wait(timeout=10)
            except Exception:pass
        for proc in reversed(children):
            if proc.poll() is None:
                proc.terminate()
                try:proc.wait(timeout=10)
                except subprocess.TimeoutExpired:proc.kill();proc.wait()
        for h in handles:h.close()
        if reverse_ready:
            try:subprocess.run(adb+['reverse','--remove',f'tcp:{tlsport}'],capture_output=True,timeout=15)
            except subprocess.TimeoutExpired:pass
        report['finished_at']=int(time.time());(base/'acceptance.json').write_text(json.dumps(report,indent=2,ensure_ascii=False)+'\n')
        print('Acceptance receipt: '+str((base/'acceptance.json').relative_to(ROOT)),flush=True)
    print(json.dumps({'passed':report['passed'],'checks':len(checks),'failed_checks':report.get('failed_checks',[]),'receipt':str(base/'acceptance.json')}),flush=True)
    if not report['passed']:raise SystemExit(1)
if __name__=='__main__':main()
