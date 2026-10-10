# The `backup` command

`zallet backup <DESTINATION>` writes a consistent snapshot of the wallet database to a
backup file, using SQLite's [Online Backup API]. Unlike copying the database file by
hand — which can capture a torn, unrestorable state if any journal or WAL sidecar files
exist — the snapshot this command produces is always a complete, self-contained database.

[Online Backup API]: https://www.sqlite.org/backup.html

If `DESTINATION` is an existing directory, the backup is written inside it as
`zallet-wallet-backup-<datetime>.db` (UTC timestamp), so repeated backups into the same
directory accumulate rather than collide. Any other path is used as the backup file's
path directly. An existing file is never overwritten: the file in the way may be your
previous good backup.

The backup is written to a temporary file first and only renamed into place after it has
passed a database integrity check and been synced to disk. If this command reports
success, the file at `DESTINATION` is a complete, verified backup; if it reports failure,
no partial file is left behind.

The command requires exclusive use of the data directory, like every command that touches
the wallet database: stop a running `zallet start` before backing up. (Scheduled backups
of a *running* wallet are planned as part of [#195], performed from inside the running
process as the Online Backup API intends.)

> **Warning**
> The wallet database holds key material in encrypted form only, so this backup is not
> sufficient to restore a wallet by itself: restoring also requires the wallet's age
> encryption identity file. The identity never changes, so back it up once — and store it
> separately from database backups, because anyone holding both can decrypt the wallet's
> key material. See [Backup and restore](../guide/backup.md) for what a complete backup
> requires, and the [threat model](../security/threat-model.md) for why backups should be
> encrypted before being uploaded anywhere.

[#195]: https://github.com/zcash/zallet/issues/195
