# Throwaway copy of the current Python mlat-server, used only as the accuracy
# baseline for replay testing. Build context is the repository root:
#   docker build -f mlat-rs/testbed/Dockerfile -t mlat-baseline:local .
FROM python:3.13-slim

RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

RUN python -m venv /opt/venv
ENV PATH=/opt/venv/bin:$PATH
RUN pip install --no-cache-dir -U pip \
    && pip install --no-cache-dir numpy scipy pykalman python-graph-core uvloop ujson Cython "setuptools<81"

WORKDIR /opt/mlat-server
COPY mlat ./mlat
COPY modes ./modes
COPY modes_cython ./modes_cython
COPY mlat-server setup.py ./
# EXTRA_CFLAGS lets the reference build disable fused multiply-add so results are bit-comparable
ARG EXTRA_CFLAGS=""
RUN CFLAGS="$EXTRA_CFLAGS" python setup.py build_ext --inplace

# same threading settings as the production container
ENV OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1 NUMEXPR_NUM_THREADS=1 \
    PYTHONOPTIMIZE=2 PYTHONWARNINGS=ignore::UserWarning
ENTRYPOINT ["python3", "/opt/mlat-server/mlat-server"]
