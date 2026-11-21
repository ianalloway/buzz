"""Persist local benchmark identities without writing their cleartext to disk."""

from __future__ import annotations

import fcntl
import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Callable

from cryptography.fernet import Fernet


def _cipher(path: Path, *, create: bool) -> Fernet:
    key = os.environ.get("BUZZ_BENCHMARK_STATE_KEY")
    if not key:
        # Only use native protected backends. Plugins may register plaintext
        # file backends, which would place the key beside the encrypted state.
        if sys.platform == "darwin":
            from keyring.backends.macOS import Keyring
        elif sys.platform.startswith("linux"):
            from keyring.backends.SecretService import Keyring
        else:
            raise RuntimeError("Set BUZZ_BENCHMARK_STATE_KEY for this platform")
        from keyring.errors import KeyringError

        store = Keyring()
        account = str(path.parent.resolve())
        service = "buzz-benchmark-state"
        try:
            key = store.get_password(service, account)
            if key is None and create:
                key = Fernet.generate_key().decode("ascii")
                store.set_password(service, account, key)
        except (KeyringError, RuntimeError) as exc:
            raise RuntimeError(
                "Unlock the OS credential store or set BUZZ_BENCHMARK_STATE_KEY"
            ) from exc
    if not key:
        raise RuntimeError("The key for the existing benchmark state is unavailable")
    return Fernet(key)


def atomic_write(path: Path, content: bytes) -> None:
    """Keep an existing file intact until its complete replacement is ready."""
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temp:
        staging = Path(temp.name)
        try:
            temp.write(content)
            temp.flush()
            os.fsync(temp.fileno())
            os.replace(staging, path)
        finally:
            staging.unlink(missing_ok=True)


def read_state(path: Path) -> dict[str, str]:
    return json.loads(_cipher(path, create=False).decrypt(path.read_bytes()))


def load_or_create_state(path: Path, generate: Callable[[], dict[str, str]]) -> dict[str, str]:
    path.parent.mkdir(mode=0o700, exist_ok=True)
    path.parent.chmod(0o700)
    # Serialize initial generation and migration, including credential-store
    # writes. Parallel starts must reuse the persisted identities.
    lock_fd = os.open(path.with_suffix(".lock"), os.O_CREAT | os.O_RDWR, 0o600)
    with os.fdopen(lock_fd, "r+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        previous = path.read_bytes() if path.exists() else None
        if previous is not None and not previous.lstrip().startswith(b"{"):
            return json.loads(_cipher(path, create=False).decrypt(previous))
        cipher = _cipher(path, create=True)
        state = json.loads(previous) if previous is not None else generate()
        encrypted = cipher.encrypt(json.dumps(state).encode())
        atomic_write(path, encrypted)
        return state
