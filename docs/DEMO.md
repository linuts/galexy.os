# Reviewer demo

A short path through a fresh disk, login, a second account, an access
card, and two consoles. The terminal is the console (COM1); `dmesg`
after login shows the kernel lines. No command here prints a password.

```sh
cargo run --release -- --display
```

Headless `cargo run --release` types in this terminal. Steps that
press F1 or F2 need the window. Those keys are PS/2. Quit a headless
session with Ctrl-A then X.

`ls` and `ls -l` list the current directory, or the directory named
on the line (`ls Desktop`, `ls -l /owner@Desktop`). A path in
another account is `/owner@Desktop`, with no slash after `@`.
`/owner@/Desktop` is not a path.

1. On a fresh disk, tty1 asks which disk to format, then a volume
   passphrase and a confirmation, then an account name and a
   password and a confirmation, then a resolver address or `skip`.
   Choose disk `1`, a passphrase, the name `owner`, and `skip`.
   Skip sends nothing. An empty line asks again; login does not
   start while the volume is locked. The name `admin` is refused.
   The next screen says to log in as the account just created. Type
   `owner`, then the password. The password line shows stars. A later
   boot asks `Volume passphrase:` for the passphrase from this step.
2. `volpass` changes the volume passphrase (current, new, confirm).
   COM1 records `[galfs] volume passphrase changed` and not the secret.
   `passwd` changes the account password later; it is not required
   before other commands on this disk.
3. `cd Desktop`, then `echo hi > note`, then `cd /`. The file has to
   exist or the later `ls` has nothing to show. `echo` only redirects
   to a name in the current directory, not to `Desktop/note`.
4. `useradd eve` and set eve's password at the single star prompt.
   There is no confirm. COM1 records `[auth] useradd user=eve`.
5. Press **F2**. tty2 still shows the passphrase prompt from boot,
   because that seat asked before tty1 chose one. Press Enter. The
   volume is already open, and the login screen appears. Log in as
   `eve`. The prompt is `eve@galexy>`.
6. `cd /owner@Desktop` prints `cd: no such directory`. Eve has no card
   yet, so the directory is not in her listing.
7. Press **F1**. Back on owner:

   ```text
   grant lr /Desktop shell2
   ```

   `l` is list and `r` is read. List is what makes `ls` and `cd` see
   the directory. Read is what makes `cat` open the file. COM1 records

   ```text
   [auth] grant actor=owner path=Desktop rights=0x5 target=shell2
   ```

   The path in that line has no leading slash. `shell2` is F2's task
   name, and it is parked on its prompt. That is still a live seat.
8. Press **F2**. `cd /owner@Desktop`, then `ls`. The line is `note`.
   `cat note` prints `hi`.
9. Press **F1**. `revoke lr /Desktop shell2`. On tty2, `cd ..` then
   `cd /owner@Desktop` prints `cd: no such directory` again.
10. `logout` on either seat returns to the login screen. The last
    logout seals the volume. Five minutes with no keys locks the seat
    instead (`[auth] idle lock`). The password opens it. It does not
    log the seat out.

Ctrl-C cancels a password prompt. Ctrl-D does nothing (it is not
end-of-file). Arrow up/down recall history on that seat only.
