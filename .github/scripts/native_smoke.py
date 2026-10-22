"""Exercise native close/relaunch, Unicode import and target-native panels."""
import json, os, platform, sqlite3, subprocess, sys, time, wave
if platform.system() == 'Linux' and not os.environ.get('QISHENG_SMOKE_DISPLAY_READY'):
    environment = dict(os.environ, QISHENG_SMOKE_DISPLAY_READY='1')
    raise SystemExit(subprocess.run(['dbus-run-session', '--', 'xvfb-run', '-a', '-s', '-screen 0 1440x1000x24', sys.executable, __file__], env=environment, timeout=130).returncode)

from pathlib import Path
root = Path(os.environ['QISHENG_CI_ROOT'])
executable = Path(os.environ['CARGO_TARGET_DIR']) / os.environ.get('QISHENG_BINARY_DIRECTORY', 'release') / ('lx-native.exe' if os.name == 'nt' else 'lx-native')
evidence = root / 'evidence'
profile = root / 'temp' / '窗口恢复 中文配置'
screenshot = evidence / 'native-window.png'
fixture = root / 'temp' / '导入测试 含空格.wav'
with wave.open(str(fixture), 'wb') as audio:
    audio.setnchannels(1)
    audio.setsampwidth(2)
    audio.setframerate(44100)
    audio.writeframes(b'\0\0' * 44100)
manager = None
primary = None
with (evidence / 'window-manager.log').open('wb') as manager_log, (evidence / 'native-window.log').open('wb') as log:
    try:
        if platform.system() == 'Linux':
            manager = subprocess.Popen(['openbox', '--sm-disable'], stdout=manager_log, stderr=subprocess.STDOUT)
            time.sleep(1)
            assert manager.poll() is None, 'Window manager did not start'
        command = [str(executable), '--data-dir', str(profile), '--audio', 'memory', '--instance-smoke', '--smoke-close', '--smoke', str(screenshot), '--smoke-ms', '8000']
        if platform.system() == 'Darwin':
            os.environ['ZSUI_NATIVE_OWNER_FILE_DIALOG_PROOF_DIR'] = str(evidence)
            command.append('--smoke-pickers')
        primary = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 6
        while not screenshot.with_suffix('.closed.json').is_file():
            assert primary.poll() is None, 'Primary window exited before launch forwarding'
            assert time.monotonic() < deadline, 'Native close was not acknowledged'
            time.sleep(0.05)
        forwarded = subprocess.run([str(executable), '--data-dir', str(profile), '--audio', 'memory', str(fixture)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
        (evidence / 'second-launch.log').write_bytes(forwarded.stdout)
        assert forwarded.returncode == 0, forwarded.stdout.decode(errors='replace')[-3000:]
        try:
            assert primary.wait(timeout=30) == 0, 'Native window failed; see native-window.log'
        except subprocess.TimeoutExpired:
            if platform.system() == 'Darwin':
                sample = subprocess.run(['sample', str(primary.pid), '1', '1', '-file', str(evidence / 'native-hang.txt')], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
                (evidence / 'native-sample.log').write_bytes(sample.stdout)
            raise
    finally:
        for process in [primary, manager]:
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
assert screenshot.exists() and screenshot.stat().st_size > 1024, 'Native screenshot missing'
interaction = json.loads(screenshot.with_suffix('.interaction.json').read_text(encoding='utf-8'))
assert interaction['relaunches_handled'] > 0, 'Second launch did not reach the UI thread'
assert interaction['background_closes'] > 0, 'Native background close did not retain the window'
database = profile / 'native-library.sqlite3'
assert database.is_file(), 'Profile database was not created'
with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as connection:
    count = connection.execute('SELECT count(*) FROM local_assets').fetchone()[0]
assert count == 1, ('Forwarded file was not imported', count)
if platform.system() == 'Darwin':
    for name in ['notepad-owner-open-panel.png', 'folder-owner-panel.png']:
        assert (evidence / name).is_file() and (evidence / name).stat().st_size > 1024, ('Native picker screenshot missing', name)
(evidence / 'native-smoke-result.json').write_text(json.dumps(dict(window='passed', backgroundClose='passed', secondLaunch='passed', unicodeFileImport='passed', importedAssets=count), indent=2), encoding='utf-8')
