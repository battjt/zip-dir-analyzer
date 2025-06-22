### zip-dir-analyze
One stop log directory analysis tool.  For now, just filter and extract; leave the collating to other tools like `sort` and `uniq`.

# Problem
We have a directory full of zips and raw files that we need to scan to analyze the behavior of our appication.  Some files are unstructured log files, while others are JSON.  Some files need to be filtered by filename while others need deeper analysis.  We need to run on IT managed machines that may not have python or even a modern Power Shell installed.

In some cases we need to anayze logs in zips that have particular JSON epressions in "manifest" files in those zips.

# The Solution
zip-dir-analyze can filter zips and raw files by name, regex pattern of the contents, JSON pattern of the contents and then extract data to be further analyzed.

Let's find all of the application log file and then from those, all of the NullPointerExceptions and sort by the most frequent.
1. find files
2. extract with regex and JQ queries
3. run file names from first run through a secondary run
4. count
5. sort by most frequent
6. report

```bash
zip-dir-analyze -jq file ./ 'manifest' '.name="application log"' | 
  zip-dir-analyze pattern -A 8 --line-delimiter '###' - '.*' 'NullPointerException'
  sort |
  uniq -c |
  sort -rn |
  sed 's/###/\n/g'
```