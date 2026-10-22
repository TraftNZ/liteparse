FROM ghcr.io/astral-sh/uv:python3.12-trixie

RUN apt-get update && apt-get install -y libgomp1 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY ./*py* .

RUN uv sync

# Download the model weights at build time so the container starts offline.
RUN uv run python3 -c "import indra_ocr; indra_ocr.load(threads=1, mode='ocr')"

EXPOSE 8831

CMD ["uv", "run", "python3", "server.py"]
