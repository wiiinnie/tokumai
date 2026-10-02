# AWS: what only an administrator can set up

The scripts run as the IAM user `tokumai-probe` (CLI profile `tokumai`), which may touch EC2
and KMS and nothing else, on purpose. The things below need the account's administrator,
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

## After the three above

```
deploy/aws/probe.sh alarm hermes-stakepool@proton.me   # topic, subscription, alarm
TAG=<image> deploy/aws/probe.sh deploy                 # the pulse units go onto the host with any deploy or `host`
```
