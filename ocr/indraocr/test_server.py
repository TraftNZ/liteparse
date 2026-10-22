import io
from types import SimpleNamespace

import pytest
from fastapi.testclient import TestClient
from PIL import Image

from server import IndraOCRServer


class MockConverter:
    def __init__(self, lines: list) -> None:
        self.lines = lines
        self.calls: list[tuple[tuple[int, ...], bool]] = []

    def ocr(self, gray, turn: bool = True) -> SimpleNamespace:
        self.calls.append((gray.shape, turn))
        return SimpleNamespace(lines=self.lines)


def _make_server(lines: list) -> IndraOCRServer:
    server = IndraOCRServer.__new__(IndraOCRServer)  # skip model load
    server.converter = MockConverter(lines)  # type: ignore
    return server


def _png_bytes(width: int = 6, height: int = 4) -> io.BytesIO:
    buffer = io.BytesIO()
    Image.new("RGB", (width, height), color=(255, 255, 255)).save(buffer, format="PNG")
    buffer.seek(0)
    return buffer


def test_health_endpoint() -> None:
    client = TestClient(_make_server([])._create_ocr_server())
    response = client.get("/health")
    assert response.status_code == 200
    assert response.json() == {"status": "healthy"}


def test_ocr_maps_lines_and_skips_orientation() -> None:
    lines = [
        SimpleNamespace(text="GROUND FLOOR PLAN", box=[10.0, 20.5, 200.0, 40.0], conf=0.97),
        SimpleNamespace(text="", box=[0.0, 0.0, 1.0, 1.0], conf=0.1),
    ]
    server = _make_server(lines)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr",
        files={"file": ("page.png", _png_bytes(), "image/png")},
        data={"language": "eng"},
    )
    assert response.status_code == 200
    results = response.json()["results"]
    assert len(results) == 1
    assert results[0]["text"] == "GROUND FLOOR PLAN"
    assert results[0]["bbox"] == [10.0, 20.5, 200.0, 40.0]
    assert results[0]["confidence"] == pytest.approx(0.97)
    assert server.converter.calls == [((4, 6), False)]  # type: ignore


def test_ocr_rejects_non_image() -> None:
    client = TestClient(_make_server([])._create_ocr_server())
    response = client.post(
        "/ocr",
        files={"file": ("page.png", io.BytesIO(b"not an image"), "image/png")},
    )
    assert response.status_code == 500
