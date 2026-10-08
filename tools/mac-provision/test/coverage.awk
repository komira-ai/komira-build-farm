# Line and branch coverage of a POSIX sh script from bash xtrace output.
#
#   awk -f coverage.awk SCRIPT TRACE
#
# TRACE is what `bash -x SCRIPT ...` wrote with PS4='+@${LINENO}@ ', over every run of
# the test suite. Each trace entry is one simple command and the line it ran on.
#
# Lines. A line is executable unless it is blank, a comment, structure alone (`fi`,
# `done ...`, `esac`, `;;`, `}`, `else`, `then`, `do`, `(`, `)`), a function header,
# a case pattern with nothing after it, or the continuation of the line before it. An
# executable line is covered when the trace holds an entry on it.
#
# Branches. Two kinds of line decide something, and each has two outcomes:
#   - `if COND; then` and `elif COND; then`: the condition ran (entries on the line
#     whose command is COND's first word) and the next executable line, the first
#     line of the `then` body, ran fewer times than the condition (the false
#     outcome) and at least once (the true outcome);
#   - `A || B` and `A && B` on one line: B ran at least once, and fewer times than A.
# Every other outcome has a line of its own: a case arm is its own line (the script
# gives each case a `*)` arm), and an `else` body is lines. So the script must not
# chain operators (`A && B || C`), put an operator in an `if` condition, or write a
# case on one line; this tool refuses those, so a branch cannot hide on a line.
#
# Prints every uncovered line and branch, then a summary; exits 1 if anything is
# uncovered or refused.

function trim(s) {
  sub(/^[ \t]+/, "", s)
  sub(/[ \t]+$/, "", s)
  return s
}

# The command word a trace entry or a command's text starts with: an assignment is
# NAME=, quotes are dropped, and a leading `!` is skipped.
function word(s, w) {
  s = trim(s)
  sub(/^! +/, "", s)
  if (match(s, /^[A-Za-z_][A-Za-z0-9_]*=/)) return substr(s, 1, RLENGTH)
  w = s
  sub(/[ \t;].*$/, "", w)
  gsub(/['"]/, "", w)
  return w
}

# The line with its single-quoted parts removed, so an operator inside an awk or
# sed program is not taken for the shell's.
function unquoted(s) {
  gsub(/'[^']*'/, "''", s)
  return s
}

function refuse(n, why) {
  printf "refused line %d: %s: %s\n", n, why, text[n]
  bad++
}

FNR == NR {
  n = FNR
  text[n] = $0
  u = trim($0)
  exec_ = 1
  if (u == "" || u ~ /^#/) exec_ = 0
  else if (u ~ /^(fi|esac|;;|}|\{|else|then|do|\(|\))$/ || u ~ /^done( |$)/) exec_ = 0
  else if (u ~ /^[A-Za-z_][A-Za-z0-9_]*\(\) \{$/) exec_ = 0
  else if (u ~ /\)$/ && u !~ /\(/) exec_ = 0
  if (cont) exec_ = 0
  cont = (u ~ /\\$/)
  isexec[n] = exec_
  last = n

  if (!exec_) next
  q = unquoted(u)
  ops = gsub(/ (\|\||&&) /, "&", q)
  if (u ~ /^case / && u !~ / in$/) refuse(n, "a case on one line")
  if (u ~ /^(if|elif) /) {
    if (u !~ /; then$/) refuse(n, "an if whose then is not at the end of its line")
    if (ops) refuse(n, "an operator in an if condition")
    kind[n] = "if"
    c = u
    sub(/^(if|elif) /, "", c)
    cword[n] = word(c)
  } else if (u ~ /^(while|until) /) {
    if (ops) refuse(n, "an operator in a loop condition")
  } else if (ops > 1) {
    refuse(n, "chained operators")
  } else if (ops == 1) {
    q = unquoted(u)
    split(q, parts, / (\|\||&&) /)
    kind[n] = (q ~ / \|\| /) ? "or" : "and"
    aword[n] = word(parts[1])
    bword[n] = word(parts[2])
  }
  next
}

# Trace entries: `+@LINE@ command` (one + per subshell level).
match($0, /^\++@[0-9]+@ /) {
  head = substr($0, 1, RLENGTH)
  sub(/^\++@/, "", head)
  sub(/@ $/, "", head)
  l = head + 0
  hit[l]++
  cnt[l, word(substr($0, RLENGTH + 1))]++
}

END {
  for (n = 1; n <= last; n++) {
    if (!isexec[n]) continue
    lines++
    if (hit[n]) covered++
    else {
      printf "uncovered line %d: %s\n", n, trim(text[n])
      continue
    }
    if (kind[n] == "if") {
      for (m = n + 1; m <= last && !isexec[m]; m++) {}
      total = cnt[n, cword[n]]
      taken = cnt[m, kind[m] == "if" ? cword[m] : word(text[m])]
      branches += 2
      if (taken > 0) bcovered++
      else printf "uncovered branch line %d (condition never true): %s\n", n, trim(text[n])
      if (total > taken) bcovered++
      else printf "uncovered branch line %d (condition never false): %s\n", n, trim(text[n])
    } else if (kind[n] == "or" || kind[n] == "and") {
      total = cnt[n, aword[n]]
      second = cnt[n, bword[n]]
      branches += 2
      if (second > 0) bcovered++
      else printf "uncovered branch line %d (%s never ran): %s\n", n, bword[n], trim(text[n])
      if (total > second) bcovered++
      else printf "uncovered branch line %d (%s always ran): %s\n", n, bword[n], trim(text[n])
    }
  }
  printf "coverage: lines %d/%d, branches %d/%d\n", covered, lines, bcovered, branches
  if (bad || covered < lines || bcovered < branches) exit 1
}
