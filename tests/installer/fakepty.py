"""Runs a command on a pseudo-terminal that presses Enter whenever it is idle.

Used by the installer tests to stand in for a person at a terminal: the installer reads its
questions from /dev/tty, and every question takes its default when Enter is pressed.

    python3 fakepty.py COMMAND [ARGS...]

The command's output goes to stdout; the exit status is the command's.
"""

import os
import pty
import select
import sys

pid, fd = pty.fork()
if pid == 0:
    os.execvp(sys.argv[1], sys.argv[1:])
while True:
    ready, _, _ = select.select([fd], [], [], 0.5)
    try:
        if ready:
            data = os.read(fd, 4096)
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.flush()
        else:
            os.write(fd, b"\n")
    except OSError:
        break
sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
