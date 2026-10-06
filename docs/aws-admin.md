# AWS: what only an administrator can set up

The scripts run as the IAM user `tokumai-probe` (CLI profile `tokumai`), which may touch EC2
and KMS (not the key policy, see 5) and nothing else, on purpose. The things below need the account's administrator,
once each, in the console. Account `946944821363`, region `eu-central-1`.

## 1. The witness bucket (rollback detection, `crates/enclave/src/witness.rs`)

S3 → Create bucket:

- Name `tokumai-book-946944821363`, region eu-central-1.
- Block all public access: on.
- Bucket Versioning: enabled (Object Lock needs it).
- Advanced settings → Object Lock: **Enable**. This can only be set at creation.

Then on the bucket, Properties → Object Lock → Edit: Default retention **enabled**, mode
**Compliance**, period **400 days**. Compliance means no identity, root included, can delete
or overwrite a mark before the 400 days are over; a lifecycle rule may expire them after.

Permissions → Bucket policy:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "TheEnclaveWritesMarksAndReads",
      "Effect": "Allow",
      "Principal": {"AWS": "arn:aws:iam::946944821363:role/tokumai-enclave-host"},
      "Action": ["s3:PutObject"],
      "Resource": "arn:aws:s3:::tokumai-book-946944821363/mark/*"
    },
    {
      "Sid": "TheEnclaveListsEverything",
      "Effect": "Allow",
      "Principal": {"AWS": "arn:aws:iam::946944821363:role/tokumai-enclave-host"},
      "Action": ["s3:ListBucket"],
      "Resource": "arn:aws:s3:::tokumai-book-946944821363"
    },
    {
      "Sid": "OnlyTheOperatorAcknowledgesARestore",
      "Effect": "Allow",
      "Principal": {"AWS": "arn:aws:iam::946944821363:user/tokumai-probe"},
      "Action": ["s3:PutObject"],
      "Resource": "arn:aws:s3:::tokumai-book-946944821363/accept/*"
    },
    {
      "Sid": "TheHostNeverAcknowledges",
      "Effect": "Deny",
      "Principal": {"AWS": "arn:aws:iam::946944821363:role/tokumai-enclave-host"},
      "Action": ["s3:PutObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::tokumai-book-946944821363/accept/*"
    }
  ]
}
```

Why it holds: the host has the role's credentials and can write marks (a denial of service
the log shows, never a rewind) but cannot write an acknowledgement; nobody can delete a
mark. The bucket's name is part of the enclave image (`deploy/enclave/Dockerfile`), so the
host cannot point the enclave elsewhere.

## 2. The host role may report the pulse (the alarm, `probe.sh alarm`)

IAM → Roles → `tokumai-enclave-host` → Add permissions → Create inline policy → JSON,
name it `tokumai-pulse`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "cloudwatch:PutMetricData",
      "Resource": "*",
      "Condition": {"StringEquals": {"cloudwatch:namespace": "tokumai"}}
    }
  ]
}
```

## 3. The operator's user may set the alarm and acknowledge a restore

IAM → Users → `tokumai-probe` → Add permissions → Create inline policy → JSON, name it
`tokumai-ops`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["sns:CreateTopic", "sns:Subscribe", "sns:ListSubscriptionsByTopic", "sns:GetTopicAttributes"],
      "Resource": "arn:aws:sns:eu-central-1:946944821363:tokumai-alarms"
    },
    {
      "Effect": "Allow",
      "Action": ["cloudwatch:PutMetricAlarm", "cloudwatch:DescribeAlarms"],
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": "s3:PutObject",
      "Resource": "arn:aws:s3:::tokumai-book-946944821363/accept/*"
    }
  ]
}
```

(`sns:CreateTopic` on a named topic ARN is how AWS scopes it; the call is idempotent.)

## 4. Daily snapshots of the book volume

Done in the console on 2026-10-02: a Data Lifecycle Manager policy "tokumai-book daily",
volumes tagged `Name=tokumai-book`, every 24 h at 03:00 UTC, 14 kept. The role
`AWSDataLifecycleManagerDefaultRole` exists. `probe.sh backups` would make the same policy
if the user were allowed `dlm:*` and `iam:PassRole` on that role.

## 5. The key admin: the one identity that may change who opens the book

Since 2026-10-05 the key policy (`deploy/aws/kms.sh`) names one principal for
`kms:PutKeyPolicy` and `kms:ScheduleKeyDeletion`, with MFA, and delegates neither to the
account — so `tokumai-probe` can deploy and seal but cannot widen the gate, and no IAM
policy can give that right to anyone. The principal must exist before the policy names it
(KMS refuses a policy with an unknown principal).

IAM → Users → Create user `tokumai-key-admin`:
- no console access needed; an access key for the CLI;
- Security credentials → assign an MFA device (a hardware key or an authenticator app);
- no permissions policy is needed for the key itself (the key policy names the user
  directly, and `sts:GetSessionToken` needs none). Add an inline policy `tokumai-key-admin`
  so the user can read the key, and nothing else:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {"Effect": "Allow", "Action": ["kms:DescribeKey", "kms:GetKeyPolicy", "kms:ListAliases"], "Resource": "*"}
  ]
}
```

On the laptop, a CLI profile `tokumai-key-admin` with that access key. An MFA session,
when a policy change is due:

```sh
aws --profile tokumai-key-admin sts get-session-token --serial-number arn:aws:iam::946944821363:mfa/<device> --token-code 123456
# → AccessKeyId / SecretAccessKey / SessionToken, valid 12 h; export them as
#   AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN and AWS_PROFILE unset, then:
deploy/aws/kms.sh allow <pcr0>
```

Then take `kms:PutKeyPolicy` and `kms:ScheduleKeyDeletion` out of `tokumai-probe`'s own
IAM policy as well (belt and braces: the key policy already refuses it).

The first `kms.sh allow` after the change is the one that installs the new shape; it may
still be run as `tokumai-probe` (the old policy allows it) and is the last that can.

Also once, with an administrator's rights (`cloudtrail:*`, `s3:*` on the new bucket,
`events:PutRule`, `events:PutTargets`, `sns:SetTopicAttributes` on `tokumai-alarms`):

```sh
AWS_PROFILE=tokumai-admin deploy/aws/kms.sh watch   # a mail for every PutKeyPolicy / ScheduleKeyDeletion / CreateGrant on the key
```

It makes the trail `tokumai` (bucket `tokumai-trail-<account>`, writing management
events, all regions, kept 400 days) — EventBridge sees CloudTrail events only through a
trail that is logging, and a new account has none — and the EventBridge rule on it. A
mail arrives a few minutes after the call. Test it with a no-op `kms.sh allow` as the key
admin.

## After the three above

```
deploy/aws/probe.sh alarm hermes-stakepool@proton.me   # topic, subscription, alarm
TAG=<image> deploy/aws/probe.sh deploy                 # the pulse units go onto the host with any deploy or `host`
```
