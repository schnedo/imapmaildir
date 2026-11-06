"""Upload of local mails: wall time and peak RSS. Usage: bench4.py <large|small> <reps> <bin>..."""
import imaplib, os, random, shutil, ssl, string, subprocess, sys, time
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../.."))
DATA = f"{REPO}/target/benchdata"
SERVER = "/tmp/imaptest/home/user/mail"
CFG = "/tmp/benchU/cfg"
SCENARIOS = {"large": (150, 10 * 1024 * 1024), "small": (6000, 2000)}
TEMPLATE = "/tmp/benchU/template"

def write_template(count, size):
    path = f"{TEMPLATE}-{count}-{size}"
    if not os.path.isdir(path):
        os.makedirs(path)
        line = "".join(random.choices(string.ascii_letters, k=76)) + "\r\n"
        body = line * (size // len(line))
        for i in range(count):
            with open(f"{path}/{1700000000+i}.M{i}P1.local:2,S", "w") as f:
                f.write(f"From: a@b.c\r\nSubject: mail {i}\r\nMessage-ID: <{i}@bench>\r\n\r\n{i}\r\n{body}")
    return path

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

def run(binary, env):
    proc = subprocess.Popen([binary, "--account", os.path.basename(binary), "--log-level", "warn", "sync"],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    _, status, usage = os.wait4(proc.pid, 0)
    err = proc.stderr.read().decode()
    if status != 0 or "ERROR" in err:
        raise SystemExit(f"sync failed: {err[-2000:]}")
    return usage

def server_count():
    ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    c = imaplib.IMAP4_SSL("localhost", 31993, ssl_context=ctx)
    c.login("user", "password")
    _, data = c.select("INBOX", readonly=True)
    c.logout()
    return int(data[0])

def upload(binary, template):
    name = os.path.basename(binary)
    shutil.rmtree(f"{DATA}/{name}", ignore_errors=True)
    shutil.rmtree(SERVER, ignore_errors=True)
    for d in ("cur", "new", "tmp"):
        os.makedirs(f"{SERVER}/{d}")
    write_config(name)
    env = dict(os.environ, XDG_CONFIG_HOME=CFG, XDG_DATA_HOME=f"{DATA}/{name}/state")
    run(binary, env)
    cur = f"{DATA}/{name}/mail/INBOX/cur"
    for f in os.listdir(template):
        shutil.copy(f"{template}/{f}", cur)
    os.sync()
    if os.environ.get("COLD") == "1":
        # drops the mails from the page cache, so that uploading reads them from disk
        for f in os.listdir(cur):
            fd = os.open(f"{cur}/{f}", os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)
    start = time.monotonic()
    usage = run(binary, env)
    elapsed = time.monotonic() - start
    return elapsed, usage.ru_maxrss / 1024, server_count(), usage.ru_utime + usage.ru_stime

if __name__ == "__main__":
    scenario, reps, bins = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    template = write_template(*SCENARIOS[scenario])
    for i in range(reps):
        for b in (bins if i % 2 == 0 else bins[::-1]):
            elapsed, rss, count, cpu = upload(b, template)
            print(f"{scenario} {os.path.basename(b)}: {elapsed:.2f}s cpu={cpu:.2f}s peak_rss={rss:.0f}MiB server_mails={count}", flush=True)
