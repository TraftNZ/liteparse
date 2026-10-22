import io
import logging
import os
import traceback
from typing import Any

import indra_ocr
import numpy as np
import uvicorn
from fastapi import FastAPI, HTTPException
from fastapi.datastructures import UploadFile
from fastapi.param_functions import File, Form
from PIL import Image
from pydantic import BaseModel

PORT = 8831
DEFAULT_THREADS = 4


class OcrResponse(BaseModel):
    results: list[Any]


class StatusResponse(BaseModel):
    status: str


class IndraOCRServer:
    def __init__(self) -> None:
        threads = int(os.environ.get("INDRA_THREADS", DEFAULT_THREADS))
        # mode="ocr" ignores any text layer: the server only ever sees rasters.
        self.converter = indra_ocr.load(threads=threads, mode="ocr")

    def recognize(self, image: Image.Image) -> list[dict[str, Any]]:
        """OCR one raster; boxes come back in its pixel coordinates.

        Orientation detection is off (turn=False): liteparse feeds upright
        page renders and crops, and Indra's page-orientation model rotates
        upright drawing sheets 180° and returns garbage for them.
        """
        gray = np.ascontiguousarray(np.asarray(image.convert("L")))
        page = self.converter.ocr(gray, False)
        return [
            {"text": line.text, "bbox": [float(v) for v in line.box], "confidence": float(line.conf)}
            for line in page.lines
            if line.text
        ]

    def _create_ocr_server(self) -> FastAPI:
        app = FastAPI()

        @app.post("/ocr")
        async def ocr_endpoint(
            file: UploadFile = File(...), language: str = Form(default="en")
        ) -> OcrResponse:
            # Indra detects the script per line, so the language hint is unused.
            del language
            try:
                image = Image.open(io.BytesIO(await file.read()))
                return OcrResponse(results=self.recognize(image))
            except Exception as e:
                logging.error("OCR failed:\n%s", traceback.format_exc())
                raise HTTPException(status_code=500, detail=str(e))

        @app.get("/health")
        def health() -> StatusResponse:
            return StatusResponse(status="healthy")

        return app

    def serve(self) -> None:
        uvicorn.run(self._create_ocr_server(), host="0.0.0.0", port=PORT)


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    logging.info("Starting server on port %d", PORT)
    IndraOCRServer().serve()
