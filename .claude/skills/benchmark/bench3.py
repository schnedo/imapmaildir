"""Initial sync wall time and peak RSS. Usage: bench3.py <scenario> <reps> <bin>..."""
import glob, os, random, shutil, sqlite3, string, subprocess, sys, threading, time
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../.."))
DATA = f"{REPO}/target/benchdata"
SERVER = "/tmp/imaptest/home/user/mail"
CFG = "/tmp/benchM/cfg"
SCENARIOS = {"large": (150, 10 * 1024 * 1024), "small": (6000, 2000)}

def write_server(count, size):
    marker = f"{SERVER}/.bench-{count}-{size}"
    if os.path.exists(marker):
        return
    shutil.rmtree(SERVER, ignore_errors=True)
    for d in ("cur", "new", "tmp"):
        os.makedirs(f"{SERVER}/{d}")
    line = "".join(random.choices(string.ascii_letters, k=76)) + "\r\n"
    body = line * (size // len(line))
    for i in range(count):
        with open(f"{SERVER}/cur/{1700000000+i}.M{i}P1.bench:2,S", "w") as f:
            f.write(f"From: a@b.c\r\nSubject: mail {i}\r\nMessage-ID: <{i}@bench>\r\n\r\n{i}\r\n{body}")
    open(marker, "w").close()

def write_config(name):
    os.makedirs(f"{CFG}/imapmaildir/accounts", exist_ok=True)
    with open(f"{CFG}/imapmaildir/accounts/{name}.toml", "w") as f:
        f.write(f'''host = "localhost"
port = 31993
server_certificate_file = "{REPO}/tests/fixtures/mock/certificate.crt"
mailboxes = ["INBOX"]
maildir_base_path = "{DATA}/{name}/mail"
[auth]
type = "Plain"
user = "user"
password_cmd = ["echo", "password"]
''')

def initial_sync(binary):
    name = os.path.basename(binary)
    shutil.rmtree(f"{DATA}/{name}", ignore_errors=True)
    write_config(name)
    env = dict(os.environ, XDG_CONFIG_HOME=CFG, XDG_DATA_HOME=f"{DATA}/{name}/state")
    start = time.monotonic()
    proc = subprocess.Popen([binary, "--account", name, "--log-level", "warn", "sync"], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    stall = float(os.environ.get("STALL", "0"))
    if stall:
        # holding the state's write lock stalls storing like a slow disk, while downloads go on
        def hold_lock():
            while not (dbs := glob.glob(f"{DATA}/{name}/state/**/imapmaildir.db", recursive=True)):
                time.sleep(0.005)
            time.sleep(0.2)
            db = sqlite3.connect(dbs[0], timeout=10)
            db.execute("begin immediate")
            time.sleep(stall)
            db.rollback(); db.close()
        threading.Thread(target=hold_lock, daemon=True).start()
    _, status, usage = os.wait4(proc.pid, 0)
    elapsed = time.monotonic() - start
    err = proc.stderr.read().decode()
    if status != 0 or "ERROR" in err:
        raise SystemExit(f"sync failed: {err[-2000:]}")
    count = len(os.listdir(f"{DATA}/{name}/mail/INBOX/cur"))
    return elapsed, usage.ru_maxrss / 1024, count

if __name__ == "__main__":
    scenario, reps, bins = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    write_server(*SCENARIOS[scenario])
    for i in range(reps):
        for b in (bins if i % 2 == 0 else bins[::-1]):
            elapsed, rss, count = initial_sync(b)
            print(f"{scenario} {os.path.basename(b)}: {elapsed:.2f}s peak_rss={rss:.0f}MiB mails={count}", flush=True)
