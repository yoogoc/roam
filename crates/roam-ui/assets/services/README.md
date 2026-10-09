# Connection identities

These assets identify storage services in the connection list and picker. They
are bundled for offline use and retain their original colours and proportions.
S3 uses the user-selected Wikimedia SVG unchanged at its original 428 × 512
dimensions. Other SVG root widths and heights are set to 256 pixels so GPUI's
image cache produces a sharp image on high-density displays; view boxes and
artwork are unchanged.

| Asset | Source | Original file |
| --- | --- | --- |
| `amazon-s3.svg` | [Amazon S3 Logo on Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Amazon-S3-Logo.svg) | [`Amazon-S3-Logo.svg`](https://upload.wikimedia.org/wikipedia/commons/b/bc/Amazon-S3-Logo.svg), unchanged |
| `google-cloud-storage.svg` | [Google Cloud product icons](https://cloud.google.com/icons) | `core-products-icons.zip`, `Unique Icons/Cloud Storage/SVG/Cloud_Storage-512-color.svg` |
| `azure-storage.svg` | [Azure product icons](https://learn.microsoft.com/en-us/azure/architecture/icons/) | `Azure_Public_Service_Icons_V24.zip`, `Icons/storage/10086-icon-service-Storage-Accounts.svg`; identifies Azure Storage used by Blob connections |
| `sharepoint.svg` | [Microsoft SharePoint product page](https://www.microsoft.com/en-us/microsoft-365/sharepoint/sharepoint-business-plans-and-pricing) | [SharePoint app icon on Microsoft's CDN](https://cdn-dynmedia-1.microsoft.com/is/content/microsoftcorp/456100-icon-sharepoint-17x17) |
| `webdav.jpg` | [WebDAV project](http://www.webdav.org/) | [`images/webdav-logo.jpg`](http://www.webdav.org/images/webdav-logo.jpg), unchanged |

The service artwork and trademarks belong to their respective owners and are
not covered by Roam's MIT license. Refer to the official source for usage terms.

Local filesystem, SFTP and NFS connections are storage/protocol types rather
than a specific vendor's product. They use Lucide `HardDrive`, `FolderLock` and
`Network` symbols respectively, provided by GPUI Kit under the ISC license.
In particular, SFTP does not imply an OpenSSH server, and NFS does not imply a
particular NAS manufacturer.
