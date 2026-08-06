import json
import os
from pathlib import Path

import pymysql

connection = pymysql.connect(
    host=os.environ["PINTAIL_WIRE_HOST"],
    port=int(os.environ["PINTAIL_WIRE_PORT"]),
    user="analytics",
    password="pk_wire_secret",
    database="analytics",
)
with connection.cursor() as cursor:
    cursor.execute(
        "SELECT COUNT(*) AS total, MIN(id) AS first_id, MAX(id) AS last_id FROM events"
    )
    aggregate = cursor.fetchone()
    corpus = []
    for query in Path("metadata.sql").read_text().split(";"):
        if query.strip():
            cursor.execute(query)
            corpus.append(cursor.fetchall())
    print(json.dumps({"aggregate": aggregate, "corpus": corpus}))
connection.close()
