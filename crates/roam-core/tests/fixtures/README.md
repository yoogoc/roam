The two SharePoint PFX files contain the same disposable RSA 2048-bit test key
and self-signed certificate, generated solely for offline tests. They are not
credentials for any service. Their password is ` pfx+test password ` (including
the leading and trailing spaces).

`sharepoint-test.pfx` uses OpenSSL's modern AES/PBKDF2 format;
`sharepoint-test-legacy.pfx` uses its legacy PKCS#12 format. Tests verify both
imports and the Microsoft Entra PS256 assertion signature. No system OpenSSL
installation is needed to run the tests.
