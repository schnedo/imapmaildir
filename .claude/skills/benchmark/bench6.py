"""Moves between mailboxes and change hooks. bins in /tmp/benchL/bin/{old,new} (like bench2.py).
Usage: python3 bench6.py <mode> N reps   (run via `nix develop -c`; dovecot via ../local-dovecot-start.sh)
  move       INBOX->Archive rename of N mails in client cur, sync once. new: UID MOVE; old: delete+upload.
             Verifies server INBOX==M-N, Archive==N. Env M (INBOX size, default 2*N).
  hookflags  server STORE +-\\Flagged on N mails in each of 3 mailboxes, sync once with hook `cat >> log`.
             Verifies hook got 3*N lines (or 3 `initial` lines never expected here). Env NOHOOK=1 -> no hook (baseline cost).
  hookslow   as hookflags but hook sleeps SLEEP (default 0.2) s; prints wall + hook invocations
             (new: batches of all mailboxes share one run; old: one run per batch).
  hookdeliver deliver N mails into client new/ of INBOX, sync once with hook; hook lines must equal N (uploaded).
"""
import imaplib, os, shutil, ssl, subprocess, sys, time
sys.path.insert(0, os.path.dirname(__file__))
import bench2
from bench2 import REPO, DATA, SERVER, CFG, mail, sync

BOXES = ["INBOX", "Archive", "Lists"]
BIN = "/tmp/benchL/bin"
HOOKDIR = "/tmp/benchL/hook"

def config(name, boxes, hook):
    os.makedirs(f"{CFG}/imapmaildir/accounts", exist_ok=True)
    hk = f'on_local_change = ["{hook}"]\n' if hook else ""
    with open(f"{CFG}/imapmaildir/accounts/{name}.toml", "w") as f:
        f.write(f'''host = "localhost"
port = 31993
server_certificate_file = "{REPO}/tests/fixtures/mock/certificate.crt"
mailboxes = {boxes!r}
maildir_base_path = "{DATA}/{name}/mail"
{hk}[auth]
type = "Plain"
user = "user"
password_cmd = ["echo", "password"]
'''.replace("'", '"'))

def box_dir(box):
    return SERVER if box == "INBOX" else f"{SERVER}/{box}"

def reset(counts):
    shutil.rmtree(SERVER, ignore_errors=True)
    shutil.rmtree(DATA, ignore_errors=True)
    for box, n in counts.items():
        for d in ("cur", "new", "tmp"):
            os.makedirs(f"{box_dir(box)}/{d}")
        for i in range(n):
            with open(f"{box_dir(box)}/cur/{1700000000+i}.M{i}P1.{box}:2,S", "w") as f:
                f.write(mail(i))

def make_hook(name, sleep):
    os.makedirs(HOOKDIR, exist_ok=True)
    log, runs = f"{HOOKDIR}/{name}.log", f"{HOOKDIR}/{name}.runs"
    for p in (log, runs):
        open(p, "w").close()
    script = f"{HOOKDIR}/{name}.sh"
    with open(script, "w") as f:
        f.write(f"#!/bin/sh\necho x >> {runs}\n" + (f"sleep {sleep}\n" if sleep else "") + f"cat >> {log}\n")
    os.chmod(script, 0o755)
    return script, log, runs

def count(path):
    with open(path) as f:
        return sum(1 for _ in f)

def files(box, sub="cur"):
    return len(os.listdir(f"{box_dir(box)}/{sub}"))

def imap():
    ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    c = imaplib.IMAP4_SSL("localhost", 31993, ssl_context=ctx)
    c.login("user", "password")
    return c

def run(b, label):
    t = sync(f"{BIN}/{b}", b)
    print(f"{label} {b}: {t:.3f}s", flush=True)
    return t

def move(n, reps):
    m = int(os.environ.get("M", 2 * n))
    for r in range(reps):
        for b in (["old", "new"] if r % 2 == 0 else ["new", "old"]):
            reset({"INBOX": m, "Archive": 0})
            config(b, ["INBOX", "Archive"], None)
            run(b, "initial-skip") if False else sync(f"{BIN}/{b}", b)
            base = f"{DATA}/{b}/mail"
            cur = sorted(os.listdir(f"{base}/INBOX/cur"))[:n]
            for f in cur:
                os.rename(f"{base}/INBOX/cur/{f}", f"{base}/Archive/cur/{f}")
            run(b, f"move{n}")
            assert files("INBOX") == m - n and files("Archive") == n, (files("INBOX"), files("Archive"))

def hook_scenario(n, reps, sleep, nohook=False):
    for r in range(reps):
        order = ["old", "new"] if r % 2 == 0 else ["new", "old"]
        res = {}
        for b in order:
            reset({box: n for box in BOXES})
            hook = None if nohook else make_hook(b, sleep)
            config(b, BOXES, hook[0] if hook else None)
            sync(f"{BIN}/{b}", b)  # initial download (reported as `initial`)
            if hook:
                for p in hook[1:]:
                    open(p, "w").close()
            c = imap()
            for box in BOXES:
                c.select(box); c.store("1:*", "+FLAGS.SILENT", r"(\Flagged)")
            c.logout()
            t = run(b, f"hookflags{n}x3" + (f" sleep={sleep}" if sleep else ""))
            if hook:
                lines, runs = count(hook[1]), count(hook[2])
                print(f"  {b}: hook lines={lines} runs={runs}", flush=True)
                assert lines == len(BOXES) * n, lines

def hookdeliver(n, reps):
    for r in range(reps):
        for b in (["old", "new"] if r % 2 == 0 else ["new", "old"]):
            reset({"INBOX": 0})
            hook = make_hook(b, 0)
            config(b, ["INBOX"], hook[0])
            sync(f"{BIN}/{b}", b)
            for i in range(n):
                with open(f"{DATA}/{b}/mail/INBOX/new/{1800000000+i}.M{i}P1.deliver", "w") as f:
                    f.write(mail(i))
            open(hook[1], "w").close()
            run(b, f"hookdeliver{n}")
            print(f"  {b}: hook lines={count(hook[1])} runs={count(hook[2])}", flush=True)

if __name__ == "__main__":
    mode, n, reps = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    if mode == "move": move(n, reps)
    elif mode == "hookflags": hook_scenario(n, reps, 0, os.environ.get("NOHOOK") == "1")
    elif mode == "hookslow": hook_scenario(n, reps, os.environ.get("SLEEP", "0.2"))
    elif mode == "hookdeliver": hookdeliver(n, reps)
