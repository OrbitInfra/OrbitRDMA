"""Each sample is from one QP between two Linux processes. Run after all builds finish."""
import os
import re
import statistics
import subprocess
import sys

cpus=sorted(os.sched_getaffinity(0))
assert len(cpus) >= 2
server_cpu,client_cpu=map(str,cpus[-2:])
print("vCPU server/client",server_cpu,client_cpu,flush=True)
spec = [("verbs","send"),("orbit","send"),("asyn","send_async"),("orbit","send_async"),("asyn","rw"),("orbit","rw"),("orbit","burst")]
if len(sys.argv) > 2:
    spec = [tuple(x.split(":")) for x in sys.argv[2:]]
rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 6
rows = {}
for k in range(rounds):
    # Each directly compared pair alternates AB/BA across runs.
    for name,kind in (spec if k%2 == 0 else list(reversed(spec))):
        binary=os.path.join(os.environ.get("BENCH_BIN_DIR","/var/tmp/peer-binaries/release"),"bench-"+name)
        with subprocess.Popen(["timeout","90","taskset","-c",server_cpu,binary,"listen","127.0.0.1:0",kind],text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as server:
            try:
                line=server.stdout.readline().strip()
                match=re.fullmatch(r"LISTEN (127[.]0[.]0[.]1:\d+)",line)
                if not match: raise RuntimeError((name,kind,line,server.stderr.read()))
                client=subprocess.run(["timeout","85","taskset","-c",client_cpu,binary,"connect",match[1],kind],text=True,capture_output=True)
                if client.returncode:
                    if server.poll() is None: server.terminate()
                    sout,serr=server.communicate(timeout=5)
                    raise RuntimeError((name,kind,client.returncode,client.stdout,client.stderr,sout,serr))
                sout,serr=server.communicate(timeout=5)
                if server.returncode:raise RuntimeError((name,kind,server.returncode,sout,serr))
                metrics=re.findall(r"^SAMPLE (\w+) (\d+) (\d+)$",client.stdout,re.M)
                assert [m[0] for m in metrics] == (["send"] if kind.startswith("send") else ["burst"] if kind=="burst" else ["write","read"])
                for op,p50,p99 in metrics:
                    key=kind if kind.startswith("send") else op
                    rows.setdefault((name,key),[]).append((int(p50),int(p99)))
                    print(k+1,name,key,p50,p99,flush=True)
            finally:
                if server.poll() is None:server.kill()
for (name,op),vals in rows.items():
    assert len(vals)==rounds
    print("MEDIAN_US",name,op,*(round(statistics.median(col)/1000,3) for col in zip(*vals)),flush=True)
