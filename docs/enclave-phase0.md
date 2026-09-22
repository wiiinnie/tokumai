# Phase 0: the platform comparison

Two probes of 2–3 PT each: today's server core in **AWS Nitro Enclaves** (packed with
[Enclaver](https://github.com/edgebitio/enclaver)) and in **Google Confidential Space**,
each with a client that verifies the attestation.

The criteria:
1. the check in the app;
2. a stable mixnet connection;
3. the database across restarts and updates;
4. cost.

The operator provides:
- **AWS:** an account with a budget alarm, and access to EC2 + KMS in eu-central-1. The
  instance type must support enclaves, e.g. `m6i.xlarge` (not t3).
- **Google Cloud:** a project with a budget alarm, and the Confidential Computing, Compute
  and KMS APIs enabled.
- `aws` / `gcloud` CLIs and Docker (or a Linux build host).

What plugs in once a platform is chosen:
- `attest::nitro` / `attest::gcp`, the verifiers. They refuse everything until built.
- a `seal::KeyProvider` that asks the platform's KMS for the data key.
- the enclave binary, built reproducibly into the platform's image format.
