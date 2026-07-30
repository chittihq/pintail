import json
import os

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
    print(json.dumps(cursor.fetchone()))
connection.close()
