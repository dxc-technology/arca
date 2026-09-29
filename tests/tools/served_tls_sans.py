"""Print the DNS SANs of the certificate a TLS server presents, one per line.

Used by `bin/test integration` to find the name under which a server running
with the user's own certificates can be verified. Verification is off here on
purpose: this only reads the certificate, the tests then verify it for real.

Usage: python served_tls_sans.py <host> <port>
"""

import socket
import ssl
import sys

from cryptography import x509


def main(host: str, port: int) -> None:
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    with socket.create_connection((host, port), timeout=5) as sock:
        with ctx.wrap_socket(sock, server_hostname=host) as tls:
            der = tls.getpeercert(binary_form=True)
    cert = x509.load_der_x509_certificate(der)
    try:
        sans = cert.extensions.get_extension_for_class(x509.SubjectAlternativeName)
    except x509.ExtensionNotFound:
        return
    for name in sans.value.get_values_for_type(x509.DNSName):
        print(name)


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]))
