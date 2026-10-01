import sqlite3
import pathlib

sql = pathlib.Path("schema/0001_init.sql").read_text(encoding="utf-8")
con = sqlite3.connect(":memory:")
con.executescript(sql)
tables = [r[0] for r in con.execute(
    "select name from sqlite_master where type='table' order by name")]
indexes = [r[0] for r in con.execute(
    "select name from sqlite_master where type='index' and name like 'idx_%' order by name")]
print("OK tables:", tables)
print("OK indexes:", indexes)
