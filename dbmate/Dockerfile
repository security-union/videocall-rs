FROM amacneil/dbmate:v1.12.0

COPY startup.sh / 
COPY db /app/db

ENV DBMATE_MIGRATIONS_DIR=/app/db/migrations 

ENTRYPOINT [ "sh", "/startup.sh" ]
