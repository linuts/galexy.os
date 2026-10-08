# Reviewer demo

A short path through login, a second account, an access card, and two
consoles. Serial (COM1) is the audit log; `dmesg` after login shows the
same lines. No command here prints a password.

```sh
cargo run
```

1. On tty1 the login screen asks for a name and a masked password.
   Type `admin`, then `admin`. The password line shows stars.
2. The shell requires `passwd` before other commands. Enter a new
   password twice. COM1 records `[auth] passwd user=admin` and not the
   secret.
3. `useradd eve` and set eve's password at the star prompt.
   COM1 records `[auth] useradd user=eve`.
4. Press **F2**. tty2 is a second login. Log in as `eve` with the
   password from step 3. The prompt is `eve@galexy>`.
5. Press **F1** to return to admin. `grant r /Desktop shell2` gives
   eve's seat read on admin's Desktop. COM1 records
   `[auth] grant actor=admin path=/Desktop rights=0x1 target=shell2`.
6. **F2**, then `ls /admin@/Desktop`. The card is what makes that path
   visible. `dmesg` prints recent kernel lines, including the grant.
7. **F1**, `revoke r /Desktop shell2`. The next `ls` on tty2 fails the
   token check.
8. `logout` on either seat returns to the login screen. The last logout
   seals the volume.

Ctrl-C cancels a password prompt. Ctrl-D does nothing (it is not
end-of-file). Arrow up/down recall history on that seat only.
