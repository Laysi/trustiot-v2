# gitleaks canary (temporary, never merged)

`fake-private-key.pem` is not a key. Its body is base64 for the sentence "This is not a
key. Fake value that proves the gitleaks CI check of Laysi/trustiot-v2#67 fails." It exists
only on the throwaway branch `ci/67-canary-gitleaks` to prove that the `gitleaks` job in
`.github/workflows/ci.yml` fails on a commit that contains something shaped like a secret.
On this branch the workflow keeps only the `gitleaks` job, so the pull request's overall
check status is the result of that job alone. The branch and its pull request are deleted
after the run.
