"""Notmuch-enabled benchmarks: deliver/upload N and idle flag updates. Usage: bench5.py <deliver|idle> <reps> [notmuch:0/1]
Binaries in /tmp/benchI/bin/{old,new}. Run inside `nix shell nixpkgs#notmuch nixpkgs#coreutils nixpkgs#python3 -c env LD_LIBRARY_PATH=<notmuch^out>/lib python3 ...`"""
import os, shutil, subprocess, sys, time, imaplib, ssl
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../.."))
DATA = f"{REPO}/target/benchdata"
SERVER = "/tmp/imaptest/home/user/mail"
CFG = "/tmp/benchI/cfg"
def reset_server(n):
    shutil.rmtree(SERVER, ignore_errors=True)
    for d in ("cur","new","tmp"): os.makedirs(f"{SERVER}/{d}")
    for i in range(n):
        with open(f"{SERVER}/cur/{1700000000+i}.M{i}P1.bench:2,S","w") as f:
            f.write(f"From: a@b.c\r\nSubject: mail {i}\r\nMessage-ID: <{i}@bench>\r\n\r\n{'x'*2000}\r\n")
def setup(b, notmuch):
    root = f"{DATA}/{b}"
    shutil.rmtree(root, ignore_errors=True); os.makedirs(f"{root}/mail")
    nm = f"{root}/notmuch-config"; extra = ""
    if notmuch:
        open(nm,"w").write(f"[database]\npath={root}/mail\n[new]\ntags=inbox;unread\n[maildir]\nsynchronize_flags=true\n")
        subprocess.run(["notmuch", f"--config={nm}", "new"], check=True, capture_output=True)
        extra = f'[notmuch]\nconfig = "{nm}"\n'
    os.makedirs(f"{CFG}/imapmaildir/accounts", exist_ok=True)
    open(f"{CFG}/imapmaildir/accounts/{b}.toml","w").write(f'''host = "localhost"
port = 31993
server_certificate_file = "{REPO}/tests/fixtures/mock/certificate.crt"
mailboxes = ["INBOX"]
maildir_base_path = "{root}/mail"
[auth]
type = "Plain"
user = "user"
password_cmd = ["echo", "password"]
{extra}''')
    return root, dict(os.environ, XDG_CONFIG_HOME=CFG, XDG_DATA_HOME=f"{root}/state")
def sync(b, env, *args):
    t=time.monotonic()
    p=subprocess.run([f"/tmp/benchI/bin/{b}","--account",b,"--log-level","warn","sync",*args],env=env,capture_output=True,text=True)
    if p.returncode or "ERROR" in p.stderr: raise SystemExit(p.stderr[-2000:])
    return time.monotonic()-t
def store(spec, flags):
    ctx=ssl.create_default_context(); ctx.check_hostname=False; ctx.verify_mode=ssl.CERT_NONE
    c=imaplib.IMAP4_SSL("localhost",31993,ssl_context=ctx); c.login("user","password"); c.select("INBOX")
    c.store(spec, flags[0], flags[1]); c.logout()
def deliver(b, notmuch, n):
    reset_server(0)
    root, env = setup(b, notmuch); sync(b, env)
    for i in range(n):
        with open(f"{root}/mail/INBOX/new/{1800000000+i}.M{i}P1.d","w") as f:
            f.write(f"From: a@b.c\r\nSubject: d {i}\r\nMessage-ID: <d{i}@bench>\r\n\r\n{'y'*2000}\r\n")
    return sync(b, env)
def idle(b, notmuch, n, total):
    reset_server(total)
    root, env = setup(b, notmuch); sync(b, env)
    p=subprocess.Popen([f"/tmp/benchI/bin/{b}","--account",b,"--log-level","warn","sync","--idle"],env=env,stderr=subprocess.DEVNULL)
    time.sleep(3); t=time.monotonic()
    store(f"1:{n}", ("+FLAGS", r"(\Flagged)"))
    cur=f"{root}/mail/INBOX/cur"
    while sum(1 for f in os.listdir(cur) if ":2," in f and "F" in f.split(":2,")[1]) < n:
        if time.monotonic()-t > 120: break
        time.sleep(0.05)
    el=time.monotonic()-t; p.terminate(); p.wait(); return el
mode=sys.argv[1]; reps=int(sys.argv[2]); nm = len(sys.argv) < 4 or sys.argv[3] == "1"
for i in range(reps):
    for b in (["old","new"] if i%2==0 else ["new","old"]):
        el = deliver(b,nm,2000) if mode=="deliver" else idle(b,nm,1000,2000)
        print(f"{mode} notmuch={nm} {b}: {el:.2f}s", flush=True)
