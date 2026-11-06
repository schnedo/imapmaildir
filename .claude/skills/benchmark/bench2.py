import os, shutil, subprocess, sys, time
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../.."))
DATA = f"{REPO}/target/benchdata"
SERVER = "/tmp/imaptest/home/user/mail"
CFG = "/tmp/benchL/cfg"
BODY = "x" * 2000

def mail(i):
    return f"From: a@b.c\r\nTo: d@e.f\r\nSubject: mail {i}\r\nMessage-ID: <{i}@bench>\r\n\r\n{BODY}\r\n"

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

def sync(binary, name):
    env = dict(os.environ, XDG_CONFIG_HOME=CFG, XDG_DATA_HOME=f"{DATA}/{name}/state")
    start = time.monotonic()
    out = subprocess.run([binary, "--account", name, "--log-level", "warn", "sync"], env=env,
                         capture_output=True, text=True)
    elapsed = time.monotonic() - start
    if out.returncode != 0 or "ERROR" in out.stderr:
        raise SystemExit(f"sync failed: {out.stderr[-2000:]}")
    return elapsed

def reset_server(count):
    shutil.rmtree(SERVER, ignore_errors=True)
    for d in ("cur", "new", "tmp"):
        os.makedirs(f"{SERVER}/{d}")
    for i in range(count):
        with open(f"{SERVER}/cur/{1700000000+i}.M{i}P1.bench:2,S", "w") as f:
            f.write(mail(i))

def resync(bins, n, reps):
    shutil.rmtree(DATA, ignore_errors=True)
    reset_server(n)
    for b in bins:
        write_config(b)
        print(f"initial {b}: {sync(f'/tmp/benchL/bin/{b}', b):.2f}s", flush=True)
    for _ in range(reps):
        for b in bins:
            print(f"resync {b}: {sync(f'/tmp/benchL/bin/{b}', b):.3f}s", flush=True)

def deliver(bins, n, reps):
    for _ in range(reps):
        for b in bins:
            shutil.rmtree(DATA, ignore_errors=True)
            reset_server(0)
            write_config(b)
            sync(f"/tmp/benchL/bin/{b}", b)
            new = f"{DATA}/{b}/mail/INBOX/new"
            for i in range(n):
                with open(f"{new}/{1800000000+i}.M{i}P1.deliver", "w") as f:
                    f.write(mail(i))
            print(f"deliver {b}: {sync(f'/tmp/benchL/bin/{b}', b):.3f}s", flush=True)

def remoteflags(bins, n, reps):
    import imaplib, ssl
    def store(flags):
        ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
        c = imaplib.IMAP4_SSL("localhost", 31993, ssl_context=ctx)
        c.login("user", "password"); c.select("INBOX")
        c.store("1:*", flags, r"(\Flagged)"); c.logout()
    shutil.rmtree(DATA, ignore_errors=True)
    reset_server(n)
    for b in bins:
        write_config(b)
        sync(f"/tmp/benchL/bin/{b}", b)
    for i in range(reps):
        # alternates adding and removing the flag, so every sync renames all mails
        store("+FLAGS.SILENT" if i % 2 == 0 else "-FLAGS.SILENT")
        for b in (bins if i % 2 == 0 else bins[::-1]):
            print(f"remoteflags {b}: {sync(f'/tmp/benchL/bin/{b}', b):.3f}s", flush=True)

if __name__ == "__main__":
    mode, n, reps = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    {"resync": resync, "deliver": deliver, "remoteflags": remoteflags}[mode](["old", "new"], n, reps)
