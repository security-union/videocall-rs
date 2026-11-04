"""Termination signals as a flag the harness checks at known points (#3246 W0); the handlers only record."""

import resource
import signal
import time

# Left at their default action: returning from a handler re-runs a real fault's instruction, abort() re-raises.
FAULTS = ("SIGSEGV", "SIGBUS", "SIGILL", "SIGFPE", "SIGABRT", "SIGSYS", "SIGTRAP", "SIGEMT")
UNCATCHABLE = ("SIGKILL", "SIGSTOP")
NON_TERMINATING = ("SIGCHLD", "SIGCONT", "SIGURG", "SIGWINCH", "SIGTSTP", "SIGTTIN", "SIGTTOU", "SIGINFO")
PYTHON_IGNORES = ("SIGPIPE", "SIGXFSZ")
UNHANDLED = {getattr(signal, n) for n in FAULTS + UNCATCHABLE + NON_TERMINATING + PYTHON_IGNORES if hasattr(signal, n)}
SIGNALS = tuple(sorted(signal.valid_signals() - UNHANDLED))
SLICE_S = 0.2
MAX_RECORDED = 16


class Interrupted(Exception):
    """Raised by Stop.check, at a known point, once a termination signal has been recorded."""


def signal_name(signum):
    try:
        return signal.Signals(signum).name
    except ValueError:
        return f"signal {signum}"


class Stop:
    def __init__(self):
        self.seen = []

    @property
    def first(self):
        return self.seen[0] if self.seen else None

    def handler(self, signum, frame):
        if len(self.seen) < MAX_RECORDED:
            self.seen.append(signum)

    def install(self):
        for sig in SIGNALS:
            signal.signal(sig, self.handler)
        resource.setrlimit(resource.RLIMIT_CORE, (0, resource.getrlimit(resource.RLIMIT_CORE)[1]))

    def requested(self):
        return bool(self.seen)

    def describe(self):
        if self.first is None:
            return "no signal"
        return signal_name(self.first) + (" and later signals" if len(self.seen) > 1 else "")

    def check(self, where):
        if self.seen:
            raise Interrupted(f"{self.describe()} during {where}")

    def pause(self, seconds, where):
        """Sleep up to seconds, in slices short enough to stop within SLICE_S of a signal; then check."""
        end = time.monotonic() + seconds
        while not self.seen and time.monotonic() < end:
            time.sleep(min(SLICE_S, max(0.0, end - time.monotonic())))
        self.check(where)


STOP = Stop()
