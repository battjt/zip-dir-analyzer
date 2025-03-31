# Brainstorming
* find zip with manifest that contains tags["application log"] extract: 
  * user:       manifest userId
  * start time: 'null-{createTime}.log'
  * stop time:  'null-{createTime}.log' file timestamp
* warn if there are multiple log files in zip



```
for f : *
  if $(jq 'tags["application log"]' f/manifest)
    print $(jq userId f/manifest), date -d "@$(regex 'f/null.(.*)\.log' '\1')", $(mtime f/null_.*.log)
```

# Solution
```
# find applicaiton logs
zip-dir-analyze --jq --quiet --zip-only $DIR 'manifest' 'tags.contains("application log")' | (
  while read f
  do (
      # extract user ID
      zip-dir-analyzer --jq "$f" 'manifest' 'user'
    
      # extract start time
      echo ${f:5:8}

      # extract stop time
      zip-dir-analyzer --time-only --file-only "$f" 'null.*' ''
    ) | tr '\n' ' '
    echo
  done
)
```