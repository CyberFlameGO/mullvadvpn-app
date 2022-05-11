#!/usr/bin/env bash
set -eu
LOCKFILE="../Cargo.lock"

lockfile_commit_hashes=$(git log --oneline --follow $LOCKFILE | awk '{print $1}')
unsigned_commits_exist="false"
for commit in $lockfile_commit_hashes;
do
    if ! $(git verify-commit $commit 2> /dev/null); then
        echo Commit $commit is not signed
        unsigned_commits_exist="true"
    fi
done

if [[ $unsigned_commits_exist == "true" ]]; then
    echo "true"
    exit 1
fi
