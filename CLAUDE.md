# Notes for Claude

## Commits are the owner's

Every commit in this repository, its author and its committer both, is
the owner's own GitHub account:

    humphrey0925 <humphrey0925@users.noreply.github.com>

Never `Claude <noreply@anthropic.com>`, nor any other account. The CLA
check (`.github/workflows/cla.yml`) looks at who wrote each commit of a
pull request, and an author that is no GitHub account can never sign:
the pull request stays failed.

- Before the first commit of a session, set the identity for this
  repository (it overrides whatever the environment set):

      git config user.name humphrey0925
      git config user.email humphrey0925@users.noreply.github.com

- Before pushing, and before opening a pull request, check every commit
  of the branch:

      git log --format='%h %an <%ae> | %cn <%ce>' origin/master..HEAD

- A commit already made as someone else, on a branch of your own, is
  rewritten as the owner's before it is pushed (or pushed again, with
  `--force-with-lease`). Never rewrite a branch someone else works on.
- A trailer in the message (`Co-Authored-By: ...`) does not change who
  the author is, and may stay.

## Commits 必須是擁有者本人的帳號

這個 repository 的每個 commit，作者（author）和提交者（committer）都必須是擁有者自己的
GitHub 帳號 `humphrey0925 <humphrey0925@users.noreply.github.com>`，不可以是
`Claude <noreply@anthropic.com>` 或其他帳號。CLA 檢查會看 pull request 每個 commit
的作者，不是 GitHub 帳號的作者永遠無法簽署，PR 會一直失敗。做法見上：每個 session 第一次
commit 前先設定 `git config user.name` 和 `user.email`，推送和開 PR 前用 `git log` 檢查。
