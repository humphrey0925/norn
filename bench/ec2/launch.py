"""Run the benchmark on an EC2 instance of its own and fetch the results.

    python bench/ec2/launch.py --bucket my-bucket            (says what it would do, and the cost)
    python bench/ec2/launch.py --bucket my-bucket --yes      (does it)

The committed checkout (HEAD) is packed and put in S3; a Spot instance of
Ubuntu 24.04 fetches it, runs bench/ec2/setup.sh, corpora.sh and run.sh on
its local NVMe disk, puts the results and its log back, and terminates
itself. It has no key pair and accepts no connections; it reaches S3
through URLs signed here that expire, so it holds no credentials. It
shuts down after --hours whatever happens. The results land in
out/ec2/<run>/.

Needs boto3 and AWS credentials (`aws configure`, or AWS_PROFILE).
"""
import argparse
import datetime
import json
import subprocess
import sys
import tarfile
import time
from pathlib import Path

import boto3

ROOT = Path(__file__).resolve().parents[2]
UBUNTU = "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"

USER_DATA = """#!/bin/bash
# (Whatever happens, the instance is gone after {hours} h.)
shutdown -h +{minutes}
exec > /var/log/norn-bench.log 2>&1
set -x
put_log() {{ curl -sS -X PUT -T /var/log/norn-bench.log "{log_put}" || true; }}
trap put_log EXIT
# The instance's own NVMe disk, for the corpora and the builds.
disk=$(lsblk -dpno NAME,MODEL | grep -i "Instance Storage" | head -1 | cut -d' ' -f1)
if [ -n "$disk" ]; then mkfs.ext4 -q -F "$disk" && mkdir -p /mnt/nvme && mount "$disk" /mnt/nvme; else mkdir -p /mnt/nvme; fi
chown ubuntu:ubuntu /mnt/nvme
sudo -u ubuntu -H bash -c '
  set -euxo pipefail
  mkdir -p /mnt/nvme/norn && cd /mnt/nvme/norn
  curl -fsSL "{code_get}" | tar -xz
  export W=/mnt/nvme/bench REPO=/mnt/nvme/norn
  bash bench/ec2/setup.sh
  . "$HOME/.cargo/env"
  bash bench/ec2/corpora.sh
  bash bench/ec2/run.sh
  tar -czf /mnt/nvme/results.tar.gz -C /mnt/nvme/bench results -C /mnt/nvme/norn out/gt
  curl -fsS -X PUT -T /mnt/nvme/results.tar.gz "{results_put}"
'
put_log
shutdown -h now
"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bucket", required=True, help="an S3 bucket of yours, in --region")
    ap.add_argument("--region", default="us-east-1")
    ap.add_argument("--type", default="c6id.4xlarge", help="instance type (one with local NVMe)")
    ap.add_argument("--hours", type=float, default=5, help="the instance shuts down after this, whatever happens")
    ap.add_argument("--on-demand", action="store_true", help="not Spot")
    ap.add_argument("--yes", action="store_true", help="launch (otherwise only say what would be done)")
    a = ap.parse_args()

    if subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, capture_output=True, text=True).stdout.strip():
        sys.exit("commit first: the instance runs HEAD, and the working tree has changes")
    head = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    run = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d-%H%M%S") + "-" + head
    session = boto3.Session(region_name=a.region)
    s3, ec2, ssm = session.client("s3"), session.client("ec2"), session.client("ssm")

    # What it costs, before anything is made.
    if a.on_demand:
        price = "on-demand (see the EC2 pricing page)"
        est = None
    else:
        hist = ec2.describe_spot_price_history(InstanceTypes=[a.type], ProductDescriptions=["Linux/UNIX"], MaxResults=20)["SpotPriceHistory"]
        prices = sorted(float(h["SpotPrice"]) for h in hist)
        est = prices[len(prices) // 2] if prices else None
        price = f"Spot, about ${est:.3f}/h now" if est else "Spot (no recent price found)"
    print(f"run {run}: {a.type} in {a.region}, {price}; at most {a.hours} h"
          + (f" (at most about ${est * a.hours:.2f}, plus a 100 GB gp3 disk while it runs)" if est else ""))
    print(f"code: HEAD {head} -> s3://{a.bucket}/norn-bench/{run}/")
    if not a.yes:
        print("(nothing done: add --yes to launch)")
        return

    prefix = f"norn-bench/{run}"
    code = ROOT / "out" / "ec2" / run / "code.tar.gz"
    code.parent.mkdir(parents=True, exist_ok=True)
    with open(code, "wb") as f:
        subprocess.run(["git", "archive", "--format=tar.gz", "HEAD"], cwd=ROOT, stdout=f, check=True)
    s3.upload_file(str(code), a.bucket, f"{prefix}/code.tar.gz")
    expires = int(a.hours * 3600) + 3600
    url = lambda op, key: s3.generate_presigned_url(op, Params={"Bucket": a.bucket, "Key": f"{prefix}/{key}"}, ExpiresIn=expires)
    user_data = USER_DATA.format(hours=a.hours, minutes=int(a.hours * 60), code_get=url("get_object", "code.tar.gz"),
                                 results_put=url("put_object", "results.tar.gz"), log_put=url("put_object", "log.txt"))
    ami = ssm.get_parameter(Name=UBUNTU)["Parameter"]["Value"]
    spec = dict(ImageId=ami, InstanceType=a.type, MinCount=1, MaxCount=1, UserData=user_data,
                InstanceInitiatedShutdownBehavior="terminate",
                BlockDeviceMappings=[{"DeviceName": "/dev/sda1", "Ebs": {"VolumeSize": 100, "VolumeType": "gp3", "DeleteOnTermination": True}}],
                MetadataOptions={"HttpTokens": "required"},
                TagSpecifications=[{"ResourceType": "instance", "Tags": [{"Key": "Name", "Value": f"norn-bench-{run}"}]}])
    if not a.on_demand:
        spec["InstanceMarketOptions"] = {"MarketType": "spot", "SpotOptions": {"SpotInstanceType": "one-time", "InstanceInterruptionBehavior": "terminate"}}
    iid = ec2.run_instances(**spec)["Instances"][0]["InstanceId"]
    print(f"launched {iid}; waiting for s3://{a.bucket}/{prefix}/results.tar.gz")

    t0 = time.time()
    while time.time() - t0 < a.hours * 3600 + 600:
        try:
            s3.head_object(Bucket=a.bucket, Key=f"{prefix}/results.tar.gz")
            break
        except s3.exceptions.ClientError:
            state = ec2.describe_instances(InstanceIds=[iid])["Reservations"][0]["Instances"][0]["State"]["Name"]
            print(f"  {int(time.time() - t0) // 60} min: {state}", flush=True)
            if state in ("terminated", "shutting-down"):
                break
            time.sleep(60)
    out = ROOT / "out" / "ec2" / run
    for key in ("results.tar.gz", "log.txt"):
        try:
            s3.download_file(a.bucket, f"{prefix}/{key}", str(out / key))
        except Exception:
            print(f"  no {key}")
    if (out / "results.tar.gz").exists():
        with tarfile.open(out / "results.tar.gz") as t:
            t.extractall(out, filter="data")
        print(f"results in {out}/results (charts: {out}/results/benchmarks)")
    else:
        print(f"no results: see {out}/log.txt")
    print(json.dumps({"run": run, "instance": iid}))


if __name__ == "__main__":
    main()
