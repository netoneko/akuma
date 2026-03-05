#[cfg(test)]
mod dns_tests {
    use crate::dns::{DnsError, is_loopback};

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(!is_loopback("example.com"));
        assert!(!is_loopback("10.0.2.15"));
    }

    #[test]
    fn dns_error_messages() {
        assert_eq!(DnsError::LookupFailed.as_str(), "DNS lookup failed");
        assert_eq!(DnsError::NoConfig.as_str(), "Network not configured");
        assert_eq!(DnsError::InvalidHost.as_str(), "Invalid hostname");
        assert_eq!(DnsError::Timeout.as_str(), "DNS query timed out");
    }
}

#[cfg(test)]
mod stats_tests {
    use crate::stats;

    #[test]
    fn stats_increment_connections() {
        let (before, _, _) = stats::get_stats();
        stats::increment_connections();
        let (after, _, _) = stats::get_stats();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn stats_bytes_tracking() {
        let (_, rx_before, tx_before) = stats::get_stats();
        stats::add_bytes_rx(100);
        stats::add_bytes_tx(200);
        let (_, rx_after, tx_after) = stats::get_stats();
        assert_eq!(rx_after, rx_before + 100);
        assert_eq!(tx_after, tx_before + 200);
    }
}

#[cfg(test)]
mod tls_tests {
    use crate::tls::TlsOptions;

    #[test]
    fn default_options_are_secure() {
        let opts = TlsOptions::new();
        assert!(!opts.insecure);
        assert!(!opts.verbose);
    }

    #[test]
    fn insecure_builder() {
        let opts = TlsOptions::new().insecure();
        assert!(opts.insecure);
        assert!(!opts.verbose);
    }

    #[test]
    fn verbose_builder() {
        let opts = TlsOptions::new().verbose();
        assert!(!opts.insecure);
        assert!(opts.verbose);
    }

    #[test]
    fn chained_builders() {
        let opts = TlsOptions::new().insecure().verbose();
        assert!(opts.insecure);
        assert!(opts.verbose);
    }
}

#[cfg(test)]
mod http_tests {
    use crate::http::{parse_url, HttpResponse};

    #[test]
    fn parse_http_url() {
        let url = parse_url("http://example.com/path").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 80);
        assert_eq!(url.path, "/path");
        assert!(!url.is_https);
    }

    #[test]
    fn parse_https_url() {
        let url = parse_url("https://example.com/secure").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.path, "/secure");
        assert!(url.is_https);
    }

    #[test]
    fn parse_url_custom_port() {
        let url = parse_url("http://example.com:8080/api").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 8080);
        assert_eq!(url.path, "/api");
    }

    #[test]
    fn parse_url_no_scheme() {
        let url = parse_url("example.com/path").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 80);
        assert!(!url.is_https);
    }

    #[test]
    fn parse_url_no_path() {
        let url = parse_url("https://example.com").unwrap();
        assert_eq!(url.path, "/");
    }

    #[test]
    fn parse_url_empty_rejects() {
        assert!(parse_url("").is_none());
        assert!(parse_url("http://").is_none());
    }

    #[test]
    fn location_header_extraction() {
        let resp = HttpResponse {
            status: 301,
            headers: "HTTP/1.1 301 Moved\r\nLocation: https://example.com/new\r\nServer: test".into(),
            body: vec![],
        };
        assert_eq!(resp.location(), Some("https://example.com/new"));
    }

    #[test]
    fn location_header_missing() {
        let resp = HttpResponse {
            status: 200,
            headers: "HTTP/1.1 200 OK\r\nServer: test".into(),
            body: vec![],
        };
        assert_eq!(resp.location(), None);
    }
}

#[cfg(test)]
mod tls_verifier_tests {
    use crate::tls_verifier::matches_hostname;

    #[test]
    fn exact_match() {
        assert!(matches_hostname("example.com", "example.com"));
    }

    #[test]
    fn wildcard_match() {
        assert!(matches_hostname("*.example.com", "sub.example.com"));
        assert!(!matches_hostname("*.example.com", "example.com"));
        assert!(!matches_hostname("*.example.com", "deep.sub.example.com"));
    }

    #[test]
    fn case_insensitive() {
        assert!(matches_hostname("Example.Com", "example.com"));
        assert!(matches_hostname("example.com", "EXAMPLE.COM"));
    }

    #[test]
    fn no_match() {
        assert!(!matches_hostname("example.com", "other.com"));
        assert!(!matches_hostname("*.example.com", "example.org"));
    }
}
