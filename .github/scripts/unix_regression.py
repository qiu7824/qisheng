"""Run native regression targets with bounded process ownership and per-target logs."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

targets = [
    'download_cleanup', 'backend_download_tasks', 'library_scan',
    'repository_library_roots', 'backend_playback_processing', 'audio_cache_latency',
    'audio_player_long_stream',
    'backend_library_relink', 'backend_library_watch', 'library_relink_actor',
    'library_authority', 'library_watch_native',
]
evidence = Path(os.environ['RUNNER_TEMP']) / 'evidence'
evidence.mkdir(exist_ok=True)
command = ['cargo', 'test', '--locked', '--no-run']
for target in targets:
    command.extend(['--test', target])

def run_owned(command, name, timeout):
    begun = time.monotonic()
    timed_out = False
    with (evidence / (name + '.log')).open('wb') as log:
        process = subprocess.Popen(
            command,
            stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            # This group contains only this invocation and its children.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
            code = 124
    return dict(target=name, exitCode=code, timedOut=timed_out, seconds=round(time.monotonic() - begun, 3))

build = run_owned(command, 'compile', 900)
print(json.dumps(build), flush=True)
if build['exitCode']:
    print((evidence / 'compile.log').read_text(encoding='utf-8', errors='replace')[-16000:])
    sys.exit(1)
results = []
for target in targets:
    result = run_owned(
        ['cargo', 'test', '--locked', '--test', target, '--', '--test-threads=1', '--nocapture'], target, 300,
    )
    results.append(result)
    print(json.dumps(result), flush=True)
    (evidence / 'results.json').write_text(json.dumps(results, indent=2), encoding='utf-8')
    if result['exitCode']:
        print((evidence / (target + '.log')).read_text(encoding='utf-8', errors='replace')[-16000:], flush=True)
sys.exit(any(result['exitCode'] != 0 for result in results))
