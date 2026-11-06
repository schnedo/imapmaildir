import imaplib, os, shutil, signal, ssl, subprocess, sys, time
N = int(os.environ.get("N", "1000"))
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../.."))
DATA = f"{REPO}/target/benchdata"
SERVER = "/tmp/imaptest/home/user/mail"
TEMPLATE = "/tmp/benchR/template"
CFG = "/tmp/benchR/cfg"

def setup_template():
    if os.path.isdir(TEMPLATE):
        return
    for d in ("cur", "new", "tmp"):
        os.makedirs(f"{TEMPLATE}/{d}")
    body = "x" * 2000
    for i in range(N):
        with open(f"{TEMPLATE}/cur/{1700000000+i}.M{i}P1.bench:2,S", "w") as f:
            f.write(f"From: a@b.c\r\nTo: d@e.f\r\nSubject: mail {i}\r\nMessage-ID: <{i}@bench>\r\n\r\n{body}\r\n")

def write_config(port):
    os.makedirs(f"{CFG}/imapmaildir/accounts", exist_ok=True)
    with open(f"{CFG}/imapmaildir/accounts/bench.toml", "w") as f:
        f.write(f'''host = "localhost"
port = {port}
server_certificate_file = "{REPO}/tests/fixtures/mock/certificate.crt"
mailboxes = ["INBOX"]
maildir_base_path = "{DATA}/mail"
[auth]
type = "Plain"
user = "user"
password_cmd = ["echo", "password"]
''')

def imap():
    ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    c = imaplib.IMAP4_SSL("localhost", 31993, ssl_context=ctx)
    c.login("user", "password")
    return c

def client_files():
    cur = f"{DATA}/mail/INBOX/cur"
    return [os.path.join(cur, n) for n in os.listdir(cur)]

def wait(cond, timeout=120):
    start = time.monotonic()
    while not cond():
        if time.monotonic() - start > timeout:
            raise SystemExit("timeout")
        time.sleep(0.01)

def flags_of(path):
    return path.rsplit(":2,", 1)[1]

def run(binary, port):
    env = dict(os.environ, XDG_CONFIG_HOME=CFG, XDG_DATA_HOME=f"{DATA}/state")
    shutil.rmtree(DATA, ignore_errors=True)
    shutil.rmtree(SERVER, ignore_errors=True)
    shutil.copytree(TEMPLATE, SERVER)
    write_config(port)
    cmd = [binary, "--account", "bench", "--log-level", "warn", "sync", "--mailbox", "INBOX"]
    subprocess.run(cmd, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    log = open(f"/tmp/benchR/client.log", "w")
    proc = subprocess.Popen(cmd + ["--idle"], env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        time.sleep(2)
        if os.environ.get("CONFLICT", "1") == "1":
            c = imap(); c.select("INBOX")
            c.store(f"1:{N}", "+FLAGS.SILENT", r"(\Answered)"); c.logout()
            wait(lambda: all("R" in flags_of(p) for p in client_files()))
            time.sleep(1)
        c = imap(); c.select("INBOX", readonly=True)
        t0 = time.monotonic()
        if os.environ.get("MODE", "flag") == "delete":
            for p in client_files():
                os.remove(p)
            def done():
                c.noop()
                _, data = c.select("INBOX", readonly=True)
                return int(data[0]) == 0
        else:
            for p in client_files():
                head, flags = p.rsplit(":2,", 1)
                os.rename(p, f"{head}:2,{''.join(sorted(flags + 'F'))}")
            def done():
                _, data = c.fetch(f"1:{N}", "(FLAGS)")
                return sum(b"\\Flagged" in d for d in data if isinstance(d, bytes)) == N
        wait(done)
        elapsed = time.monotonic() - t0
        c.logout()
    finally:
        proc.send_signal(signal.SIGTERM); proc.wait()
        log.close()
    text = open("/tmp/benchR/client.log").read()
    rejected = "changed on server" in text
    errors = [l for l in text.splitlines() if "ERROR" in l or "panic" in l]
    return elapsed, rejected, errors

if __name__ == "__main__":
    setup_template()
    binary, port = sys.argv[1], int(sys.argv[2])
    elapsed, rejected, errors = run(binary, port)
    print(f"{os.path.basename(binary)} port={port} elapsed={elapsed:.3f}s rejected={rejected} errors={len(errors)}")
    for e in errors[:3]:
        print("  ", e)
